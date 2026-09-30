//! Bounded, streaming, read-only search. No global file list and no subprocess per file.
//! Blocking filesystem work has cooperative cancellation and a process-wide concurrency cap.
use crate::workspace_tools::SearchTextEntry;
use ignore::{WalkBuilder, gitignore::GitignoreBuilder};
use regex::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::fs::File;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::Command as StdCommand;
use std::sync::{
    Arc, Mutex, OnceLock,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;

const LINE_BYTES: usize = 16 * 1024;
const MAX_ENTRIES: usize = 50_000;
const WORKERS: usize = 4;

#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Request {
    pub pattern: String,
    pub path: Option<String>,
    pub glob: Option<String>,
    pub fixed_strings: bool,
    pub case_insensitive: bool,
    pub context: Option<usize>,
    pub before: Option<usize>,
    pub after: Option<usize>,
    pub max_matches: Option<usize>,
    pub max_matches_per_file: Option<usize>,
    pub include_hidden: bool,
    pub no_ignore: bool,
    pub source_only: bool,
    pub timeout_ms: u64,
    pub max_bytes: u64,
    pub max_file_bytes: u64,
    pub max_response_bytes: usize,
    #[cfg(test)]
    #[serde(skip)]
    pub test_delay_ms: u64,
}
impl Default for Request {
    fn default() -> Self {
        Self {
            pattern: String::new(),
            path: None,
            glob: None,
            fixed_strings: false,
            case_insensitive: false,
            context: None,
            before: None,
            after: None,
            max_matches: None,
            max_matches_per_file: None,
            include_hidden: false,
            no_ignore: false,
            source_only: true,
            timeout_ms: 5_000,
            max_bytes: 32 * 1024 * 1024,
            max_file_bytes: 2 * 1024 * 1024,
            max_response_bytes: 128 * 1024,
            #[cfg(test)]
            test_delay_ms: 0,
        }
    }
}
impl Request {
    pub fn validate(&self) -> Result<(), String> {
        if self.pattern.trim().is_empty() || self.pattern.len() > 4096 {
            return Err(
                "code: INVALID_PATTERN\nmessage: pattern must contain 1..4096 bytes".into(),
            );
        }
        if self
            .path
            .as_ref()
            .is_some_and(|s| s.len() > 4096 || s.contains('\0'))
            || self
                .glob
                .as_ref()
                .is_some_and(|s| s.len() > 4096 || s.contains('\0'))
        {
            return Err(
                "code: INVALID_PATH\nmessage: path or glob is too large or contains NUL".into(),
            );
        }
        for (name, value) in [
            ("context", self.context),
            ("before", self.before),
            ("after", self.after),
        ] {
            if value.is_some_and(|n| n > 20) {
                return Err(format!("{name} must be between 0 and 20"));
            }
        }
        for (name, value) in [
            ("max_matches", self.max_matches),
            ("max_matches_per_file", self.max_matches_per_file),
        ] {
            if value.is_some_and(|n| n == 0 || n > 500) {
                return Err(format!("{name} must be between 1 and 500"));
            }
        }
        if !(1..=30_000).contains(&self.timeout_ms)
            || !(1..=256 * 1024 * 1024).contains(&self.max_bytes)
            || !(1..=16 * 1024 * 1024).contains(&self.max_file_bytes)
            || !(4096..=256 * 1024).contains(&self.max_response_bytes)
        {
            return Err("code: INVALID_SEARCH_BUDGET\nmessage: invalid time, file, input or response budget".into());
        }
        if self.response_reserve() >= self.max_response_bytes {
            return Err("code: INVALID_SEARCH_BUDGET\nmessage: response budget is too small for the query metadata".into());
        }
        Ok(())
    }
    fn response_reserve(&self) -> usize {
        2304 + serde_json::to_vec(&json!([self.pattern, self.path]))
            .map(|v| v.len())
            .unwrap_or(usize::MAX / 2)
    }
    fn matcher(&self) -> Result<Regex, String> {
        let pattern = if self.fixed_strings {
            regex::escape(&self.pattern)
        } else {
            self.pattern.clone()
        };
        RegexBuilder::new(&pattern)
            .case_insensitive(self.case_insensitive)
            .size_limit(2 * 1024 * 1024)
            .dfa_size_limit(2 * 1024 * 1024)
            .build()
            .map_err(|e| format!("code: INVALID_PATTERN\nmessage: {e}"))
    }
    fn before(&self) -> usize {
        self.context.or(self.before).unwrap_or(0)
    }
    fn after(&self) -> usize {
        self.context.or(self.after).unwrap_or(0)
    }
}
#[derive(Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Diagnostics {
    pub stop_reasons: Vec<String>,
    pub visited_entries: usize,
    pub files_opened: usize,
    pub bytes_read: u64,
    pub skipped_large_files: usize,
    pub skipped_binary_files: usize,
    pub skipped_long_lines: usize,
    pub skipped_invalid_utf8_lines: usize,
    pub io_errors: usize,
    pub scope_excluded_entries: usize,
    pub result_bytes: usize,
    pub prepare_ms: f64,
    pub scan_ms: f64,
    pub elapsed_ms: f64,
    pub worker_stopped: bool,
}
#[derive(Clone)]
struct Output {
    entries: Vec<SearchTextEntry>,
    matches: usize,
    diag: Diagnostics,
    backend: &'static str,
    backend_note: &'static str,
}
impl Output {
    fn new() -> Self {
        Self {
            entries: vec![],
            matches: 0,
            diag: Diagnostics::default(),
            backend: "rust-stream",
            backend_note: "Bounded embedded ignore/regex engine; no rg/grep subprocess. Limits count content bytes, not filesystem metadata IO.",
        }
    }
    fn reason(&mut self, reason: &str) {
        if !self.diag.stop_reasons.iter().any(|s| s == reason) {
            self.diag.stop_reasons.push(reason.into());
        }
    }
    fn finish(mut self, req: &Request, start: Instant, worker_stopped: bool) -> Value {
        self.diag.elapsed_ms = ms(start.elapsed());
        self.diag.worker_stopped = worker_stopped;
        json!({"toolName":"search","searchPattern":req.pattern,
            "searchPath":req.path.as_deref().unwrap_or("."),"searchBackend":self.backend,
            "searchBackendNote":self.backend_note,
            "matchCount":self.matches,"searchTruncated":!self.diag.stop_reasons.is_empty(),
            "searchLimit":req.max_matches.unwrap_or(100),"searchResults":self.entries,
            "searchDiagnostics":self.diag,
            "searchScope":{"includeHidden":req.include_hidden,"respectIgnores":!req.no_ignore,
                "sourceOnly":req.source_only && !req.no_ignore,"followSymlinks":false,
                "maxDepth":64,"maxLineBytes":LINE_BYTES},
            "searchBudget":{"timeoutMs":req.timeout_ms,"maxBytes":req.max_bytes,
                "maxFileBytes":req.max_file_bytes,"maxResponseBytes":req.max_response_bytes,
                "maxVisitedEntries":MAX_ENTRIES}})
    }
}
fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}
struct CancelOnDrop(Arc<AtomicBool>);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}
struct Stopped(Arc<AtomicBool>);
impl Drop for Stopped {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}
fn workers() -> Arc<Semaphore> {
    static SEM: OnceLock<Arc<Semaphore>> = OnceLock::new();
    SEM.get_or_init(|| Arc::new(Semaphore::new(WORKERS)))
        .clone()
}

pub async fn search(workspace: String, req: Request) -> Result<Value, String> {
    search_with(workspace, req, workers()).await
}
async fn search_with(
    workspace: String,
    req: Request,
    semaphore: Arc<Semaphore>,
) -> Result<Value, String> {
    req.validate()?;
    let start = Instant::now();
    let permit = semaphore.try_acquire_owned()
        .map_err(|_|"code: SEARCH_BUSY\nmessage: search workers are occupied; no unbounded queue is created")?;
    let cancel = Arc::new(AtomicBool::new(false));
    let _cancel_guard = CancelOnDrop(cancel.clone());
    let stopped = Arc::new(AtomicBool::new(false));
    let progress = Arc::new(Mutex::new(Output::new()));
    let worker_req = req.clone();
    let worker_progress = progress.clone();
    let worker_stopped = stopped.clone();
    let worker_cancel = cancel.clone();
    let mut task = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let _stopped = Stopped(worker_stopped);
        scan(
            &workspace,
            &worker_req,
            start,
            worker_cancel,
            worker_progress,
        )
    });
    match tokio::time::timeout(Duration::from_millis(req.timeout_ms), &mut task).await {
        Ok(result) => {
            let output =
                result.map_err(|e| format!("code: SEARCH_WORKER_ERROR\nmessage: {e}"))??;
            Ok(output.finish(&req, start, true))
        }
        Err(_) => {
            cancel.store(true, Ordering::Release);
            let mut output = lock(&progress).clone();
            output.reason("time_budget");
            // Never call abort and claim a blocking filesystem syscall stopped.
            Ok(output.finish(&req, start, stopped.load(Ordering::Acquire)))
        }
    }
}
struct Scan<'a> {
    req: &'a Request,
    output: Output,
    start: Instant,
    cancel: Arc<AtomicBool>,
    progress: Arc<Mutex<Output>>,
    last_publish: Instant,
    matcher: Regex,
}
impl Scan<'_> {
    fn check(&mut self) -> bool {
        let reason = if self.start.elapsed() >= Duration::from_millis(self.req.timeout_ms) {
            Some("time_budget")
        } else if self.cancel.load(Ordering::Acquire) {
            Some("cancelled")
        } else {
            None
        };
        if let Some(reason) = reason {
            self.output.reason(reason);
            self.publish();
            return false;
        }
        if self.last_publish.elapsed() >= Duration::from_millis(25) {
            self.publish();
        }
        true
    }
    fn publish(&mut self) {
        self.output.diag.scan_ms =
            (ms(self.start.elapsed()) - self.output.diag.prepare_ms).max(0.0);
        *lock(&self.progress) = self.output.clone();
        self.last_publish = Instant::now();
    }
    fn emit(&mut self, path: &str, line: usize, text: &str, is_context: bool) -> bool {
        let entry = SearchTextEntry {
            path: path.into(),
            line,
            text: text.into(),
            is_context,
        };
        let cost = serde_json::to_vec(&entry)
            .map(|v| v.len() + 1)
            .unwrap_or(usize::MAX);
        // Metadata is small and bounded separately; reserve 3 KiB for its JSON envelope.
        if self.output.diag.result_bytes.saturating_add(cost)
            > self.req.max_response_bytes - self.req.response_reserve()
        {
            self.output.reason("response_budget");
            return false;
        }
        self.output.diag.result_bytes += cost;
        self.output.matches += usize::from(!is_context);
        self.output.entries.push(entry);
        true
    }
    fn file(&mut self, root: &Path, path: &Path) -> Result<bool, String> {
        let meta = std::fs::symlink_metadata(path).map_err(|e| e.to_string())?;
        if !meta.is_file() || meta.file_type().is_symlink() {
            return Ok(true);
        }
        if meta.len() > self.req.max_file_bytes {
            self.output.diag.skipped_large_files += 1;
            self.output.reason("file_size_limit");
            return Ok(true);
        }
        // The walker starts from a canonical path below the canonical workspace root,
        // does not follow symlinks, and symlink files are rejected above. Re-canonicalizing
        // every file is therefore redundant and disproportionately expensive on Windows/NTFS.
        if !path.starts_with(root) {
            self.output.reason("outside_scope");
            return Ok(true);
        }
        let mut file = File::open(path).map_err(|e| e.to_string())?;
        if !file.metadata().map_err(|e| e.to_string())?.is_file() {
            self.output.reason("special_file");
            return Ok(true);
        }
        self.output.diag.files_opened += 1;
        let rel = path
            .strip_prefix(root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");
        let mut history = VecDeque::<(usize, String)>::new();
        let mut line = Vec::new();
        let mut too_long = false;
        let mut number = 0usize;
        let mut last_emitted = 0usize;
        let mut tail = 0usize;
        let mut file_matches = 0usize;
        let mut consumed = 0u64;
        let mut buffer = [0u8; 8192];
        let results_before = self.output.entries.len();
        let matches_before = self.output.matches;
        loop {
            if !self.check() {
                return Ok(false);
            }
            let remaining = self
                .req
                .max_bytes
                .saturating_sub(self.output.diag.bytes_read)
                .min(self.req.max_file_bytes.saturating_sub(consumed));
            if remaining == 0 {
                self.output.reason(if consumed >= self.req.max_file_bytes {
                    "file_size_limit"
                } else {
                    "byte_budget"
                });
                return Ok(consumed >= self.req.max_file_bytes
                    && self.output.diag.bytes_read < self.req.max_bytes);
            }
            let cap = buffer.len().min(remaining as usize);
            let n = match file.read(&mut buffer[..cap]) {
                Ok(n) => n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.to_string()),
            };
            consumed += n as u64;
            self.output.diag.bytes_read += n as u64;
            if buffer[..n].contains(&0) {
                self.output.entries.truncate(results_before);
                self.output.matches = matches_before;
                self.output.diag.result_bytes = self
                    .output
                    .entries
                    .iter()
                    .map(|e| serde_json::to_vec(e).unwrap().len() + 1)
                    .sum();
                self.output.diag.skipped_binary_files += 1;
                return Ok(true);
            }
            let eof = n == 0;
            for byte in buffer[..n].iter().copied().chain(eof.then_some(b'\n')) {
                if byte != b'\n' {
                    if line.len() < LINE_BYTES {
                        line.push(byte);
                    } else {
                        too_long = true;
                    }
                    continue;
                }
                if eof && line.is_empty() && !too_long {
                    break;
                }
                number += 1;
                if !self.check() {
                    return Ok(false);
                }
                if too_long {
                    self.output.diag.skipped_long_lines += 1;
                    self.output.reason("line_size_limit");
                    history.clear();
                    tail = tail.saturating_sub(1);
                    line.clear();
                    too_long = false;
                    continue;
                }
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                let text = match std::str::from_utf8(&line) {
                    Ok(s) => s.to_string(),
                    Err(_) => {
                        self.output.diag.skipped_invalid_utf8_lines += 1;
                        self.output.reason("invalid_utf8");
                        history.clear();
                        line.clear();
                        tail = tail.saturating_sub(1);
                        continue;
                    }
                };
                let allowed = self
                    .req
                    .max_matches_per_file
                    .is_none_or(|max| file_matches < max);
                let matched = allowed && self.matcher.is_match(&text);
                if matched {
                    if self.output.matches >= self.req.max_matches.unwrap_or(100) {
                        self.output.reason("match_limit");
                        return Ok(false);
                    }
                    for (previous, value) in &history {
                        if *previous > last_emitted {
                            if !self.emit(&rel, *previous, value, true) {
                                return Ok(false);
                            }
                            last_emitted = *previous;
                        }
                    }
                    if !self.emit(&rel, number, &text, false) {
                        return Ok(false);
                    }
                    last_emitted = number;
                    file_matches += 1;
                    tail = self.req.after();
                } else if tail > 0 {
                    if !self.emit(&rel, number, &text, true) {
                        return Ok(false);
                    }
                    last_emitted = number;
                    tail -= 1;
                }
                history.push_back((number, text));
                while history.len() > self.req.before() {
                    history.pop_front();
                }
                line.clear();
                if tail == 0 && self.output.matches >= self.req.max_matches.unwrap_or(100) {
                    self.output.reason("match_limit");
                    return Ok(false);
                }
                if tail == 0
                    && self
                        .req
                        .max_matches_per_file
                        .is_some_and(|m| file_matches >= m)
                {
                    self.output.reason("per_file_limit");
                    return Ok(true);
                }
            }
            if eof {
                return Ok(true);
            }
        }
    }
}

fn git_output(repo: &Path, args: &[&str]) -> Option<std::process::Output> {
    StdCommand::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .ok()
}

fn nul_paths(output: &[u8], repo_root: &Path) -> Vec<PathBuf> {
    output
        .split(|byte| *byte == 0)
        .filter(|item| !item.is_empty())
        .map(|item| repo_root.join(String::from_utf8_lossy(item).as_ref()))
        .collect()
}

fn has_hidden_component(path: &Path) -> bool {
    path.components().any(|component| match component {
        Component::Normal(name) => name
            .to_str()
            .is_some_and(|name| name.starts_with('.') && name != "." && name != ".."),
        _ => false,
    })
}

fn is_source_only_excluded(relative_to_start: &Path) -> bool {
    const HEAVY: &[&str] = &[
        "node_modules",
        "target",
        "dist",
        "build",
        ".git",
        ".next",
        ".venv",
        "__pycache__",
        ".catdesk-worktrees",
        ".worktrees",
        "coverage",
        ".cache",
        ".turbo",
    ];
    if relative_to_start
        .components()
        .any(|component| match component {
            Component::Normal(name) => name
                .to_str()
                .is_some_and(|name| HEAVY.iter().any(|heavy| name.eq_ignore_ascii_case(heavy))),
            _ => false,
        })
    {
        return true;
    }
    let lower = relative_to_start
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    [".tar", ".tar.gz", ".tgz", ".zip", ".7z", ".iso"]
        .iter()
        .any(|suffix| lower.ends_with(suffix))
}

fn custom_ignore_matcher(
    repo_root: &Path,
    start: &Path,
    indexed: &[PathBuf],
) -> Option<ignore::gitignore::Gitignore> {
    let mut ignore_files = Vec::<PathBuf>::new();

    // Ancestor custom ignore files can affect the selected search root.
    let mut cursor = if start.is_dir() {
        start.to_path_buf()
    } else {
        start.parent()?.to_path_buf()
    };
    loop {
        for name in [".catdeskignore", ".rgignore"] {
            let candidate = cursor.join(name);
            if candidate.is_file() {
                ignore_files.push(candidate);
            }
        }
        if cursor == repo_root {
            break;
        }
        let parent = cursor.parent()?.to_path_buf();
        if !parent.starts_with(repo_root) {
            break;
        }
        cursor = parent;
    }

    // Nested custom ignore files affect their descendants. Git's index gives us
    // a cheap directory inventory without walking generated trees.
    for path in indexed {
        if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| matches!(name, ".catdeskignore" | ".rgignore"))
        {
            ignore_files.push(path.clone());
        }
    }

    ignore_files.sort_by_key(|path| path.components().count());
    ignore_files.dedup();

    if ignore_files.is_empty() {
        return None;
    }

    let mut builder = GitignoreBuilder::new(repo_root);
    let mut added = false;
    for path in ignore_files {
        if builder.add(path).is_none() {
            added = true;
        }
    }
    added.then(|| builder.build().ok()).flatten()
}

fn git_prefilter_candidates(start: &Path, req: &Request) -> Option<Vec<PathBuf>> {
    // Keep the optimization narrow: literal coding searches with normal ignore
    // semantics. Regex/glob/no-ignore paths retain the exhaustive bounded walker.
    if !req.fixed_strings || req.no_ignore || req.glob.is_some() || req.pattern.contains('\n') {
        return None;
    }

    let probe = if start.is_dir() {
        start
    } else {
        start.parent()?
    };
    let top = git_output(probe, &["rev-parse", "--show-toplevel"])?;
    if !top.status.success() {
        return None;
    }
    let repo_text = String::from_utf8_lossy(&top.stdout);
    let repo_root = PathBuf::from(repo_text.trim()).canonicalize().ok()?;
    if !start.starts_with(&repo_root) {
        return None;
    }

    let pathspec = start.strip_prefix(&repo_root).ok().map(|path| {
        if path.as_os_str().is_empty() {
            ".".to_string()
        } else {
            path.to_string_lossy().replace('\\', "/")
        }
    })?;

    let mut grep_args = vec!["grep", "-z", "-l", "-F", "-I"];
    if req.case_insensitive {
        grep_args.push("-i");
    }
    grep_args.extend(["-e", req.pattern.as_str(), "--", pathspec.as_str()]);
    let tracked = git_output(&repo_root, &grep_args)?;
    // git grep returns 1 when there are no matches.
    if !tracked.status.success() && tracked.status.code() != Some(1) {
        return None;
    }

    let indexed = git_output(
        &repo_root,
        &[
            "ls-files",
            "-c",
            "-o",
            "--exclude-standard",
            "-z",
            "--",
            pathspec.as_str(),
        ],
    )?;
    if !indexed.status.success() {
        return None;
    }
    let indexed_paths = nul_paths(&indexed.stdout, &repo_root);
    let custom_ignore = custom_ignore_matcher(&repo_root, start, &indexed_paths);

    let untracked = git_output(
        &repo_root,
        &[
            "ls-files",
            "-o",
            "--exclude-standard",
            "-z",
            "--",
            pathspec.as_str(),
        ],
    )?;
    if !untracked.status.success() {
        return None;
    }

    let mut candidates = nul_paths(&tracked.stdout, &repo_root);
    candidates.extend(nul_paths(&untracked.stdout, &repo_root));
    candidates.sort();
    candidates.dedup();

    if candidates.len() > MAX_ENTRIES {
        return None;
    }

    let start_dir = if start.is_dir() {
        start
    } else {
        start.parent()?
    };
    candidates.retain(|path| {
        if !path.starts_with(start_dir) || !path.is_file() {
            return false;
        }
        let rel_start = path.strip_prefix(start_dir).unwrap_or(path);
        if !req.include_hidden && has_hidden_component(rel_start) {
            return false;
        }
        if req.source_only && is_source_only_excluded(rel_start) {
            return false;
        }
        if custom_ignore
            .as_ref()
            .is_some_and(|matcher| matcher.matched_path_or_any_parents(path, false).is_ignore())
        {
            return false;
        }
        true
    });

    Some(candidates)
}

fn scan(
    workspace: &str,
    req: &Request,
    start_time: Instant,
    cancel: Arc<AtomicBool>,
    progress: Arc<Mutex<Output>>,
) -> Result<Output, String> {
    #[cfg(test)]
    if req.test_delay_ms > 0 {
        std::thread::sleep(Duration::from_millis(req.test_delay_ms));
    }
    let root = Path::new(workspace)
        .canonicalize()
        .map_err(|e| format!("code: INVALID_WORKSPACE\nmessage: {e}"))?;
    let input = req.path.as_deref().unwrap_or(".");
    let candidate = if Path::new(input).is_absolute() {
        PathBuf::from(input)
    } else {
        root.join(input)
    };
    let start = candidate
        .canonicalize()
        .map_err(|e| format!("code: INVALID_PATH\nmessage: {e}"))?;
    if !start.starts_with(&root) || (!start.is_file() && !start.is_dir()) {
        return Err("code: PATH_OUTSIDE_WORKSPACE\nmessage: search path must be a file or directory in the workspace".into());
    }
    let matcher = req.matcher()?;
    let glob = if let Some(g) = req.glob.as_deref().filter(|s| !s.trim().is_empty()) {
        let mut b = globset::GlobSetBuilder::new();
        b.add(globset::Glob::new(g).map_err(|e| format!("code: INVALID_GLOB\nmessage: {e}"))?);
        if !g.contains('/') && !g.contains('\\') {
            b.add(globset::Glob::new(&format!("**/{g}")).map_err(|e| e.to_string())?);
        }
        Some(b.build().map_err(|e| e.to_string())?)
    } else {
        None
    };
    let mut run = Scan {
        req,
        output: Output::new(),
        start: start_time,
        cancel: cancel.clone(),
        progress,
        last_publish: Instant::now(),
        matcher,
    };
    run.output.diag.prepare_ms = ms(start_time.elapsed());
    if !run.check() {
        return Ok(run.output);
    }
    let scan_start = Instant::now();

    if let Some(candidates) = git_prefilter_candidates(&start, req) {
        run.output.backend = "git-prefilter+rust-stream";
        run.output.backend_note = "Git index narrows tracked literal matches and adds untracked non-ignored files; CatDesk re-scans candidates for exact context/result semantics.";
        run.output.diag.visited_entries = candidates.len();
        for path in candidates {
            if !run.check() {
                break;
            }
            match run.file(&root, &path) {
                Ok(true) => {}
                Ok(false) => break,
                Err(_) => {
                    run.output.diag.io_errors += 1;
                    run.output.reason("io_error");
                }
            }
            run.publish();
        }
        run.check();
        run.output.diag.scan_ms = ms(scan_start.elapsed());
        run.publish();
        return Ok(run.output);
    }

    let excluded = Arc::new(AtomicUsize::new(0));
    let excluded_filter = excluded.clone();
    let visited = Arc::new(AtomicUsize::new(0));
    let visited_filter = visited.clone();
    let source_only = req.source_only && !req.no_ignore;
    let timeout = req.timeout_ms;
    let mut walker = WalkBuilder::new(&start);
    walker
        .follow_links(false)
        .max_depth(Some(64))
        .hidden(!req.include_hidden)
        .parents(!req.no_ignore)
        .ignore(!req.no_ignore)
        .git_global(!req.no_ignore)
        .git_ignore(!req.no_ignore)
        .git_exclude(!req.no_ignore)
        .filter_entry(move |e| {
            if cancel.load(Ordering::Acquire)
                || start_time.elapsed() >= Duration::from_millis(timeout)
            {
                // Yield a sentinel so the outer loop can break; filtering everything out
                // would keep enumerating an enormous directory inside Walk::next().
                return true;
            }
            let n = visited_filter.fetch_add(1, Ordering::Relaxed);
            if n >= MAX_ENTRIES {
                return true;
            }
            if source_only
                && e.depth() > 0
                && e.file_type().is_some_and(|t| t.is_dir())
                && matches!(
                    e.file_name().to_str(),
                    Some(
                        "node_modules"
                            | "target"
                            | "dist"
                            | "build"
                            | ".git"
                            | ".next"
                            | ".venv"
                            | "__pycache__"
                            | ".catdesk-worktrees"
                            | ".worktrees"
                            | "coverage"
                            | ".cache"
                            | ".turbo"
                    )
                )
            {
                excluded_filter.fetch_add(1, Ordering::Relaxed);
                return false;
            }
            true
        });
    if !req.no_ignore {
        walker.add_custom_ignore_filename(".rgignore");
        walker.add_custom_ignore_filename(".catdeskignore");
    }
    for item in walker.build() {
        if !run.check() {
            break;
        }
        run.output.diag.visited_entries = visited.load(Ordering::Relaxed).min(MAX_ENTRIES);
        if visited.load(Ordering::Relaxed) >= MAX_ENTRIES {
            run.output.reason("entry_budget");
            break;
        }
        let entry = match item {
            Ok(e) => e,
            Err(_) => {
                run.output.diag.io_errors += 1;
                run.output.reason("io_error");
                continue;
            }
        };
        if entry.error().is_some() {
            run.output.diag.io_errors += 1;
            run.output.reason("ignore_error");
        }
        let path = entry.path();
        if entry.file_type().is_some_and(|t| t.is_dir()) && entry.depth() >= 64 {
            run.output.reason("depth_limit");
        }
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        if glob
            .as_ref()
            .is_some_and(|g| !g.is_match(path.strip_prefix(&root).unwrap_or(path)))
        {
            continue;
        }
        match run.file(&root, path) {
            Ok(true) => {}
            Ok(false) => break,
            Err(_) => {
                run.output.diag.io_errors += 1;
                run.output.reason("io_error");
            }
        }
        run.publish();
    }
    if visited.load(Ordering::Relaxed) >= MAX_ENTRIES {
        run.output.reason("entry_budget");
    }
    run.check();
    run.output.diag.scope_excluded_entries = excluded.load(Ordering::Relaxed);
    run.output.diag.scan_ms = ms(scan_start.elapsed());
    run.publish();
    Ok(run.output)
}

#[cfg(test)]
mod tests;
