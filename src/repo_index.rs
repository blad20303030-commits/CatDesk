use regex::Regex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const INDEX_FORMAT_VERSION: u32 = 1;
const MEMORY_TTL: Duration = Duration::from_secs(2);

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SymbolHit {
    pub kind: String,
    pub path: String,
    pub line: usize,
    pub context: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FileFingerprint {
    size: u64,
    modified_ns: u128,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FileEntry {
    fingerprint: FileFingerprint,
    symbols: Vec<(String, SymbolHit)>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DiskIndex {
    version: u32,
    root: String,
    files: HashMap<String, FileEntry>,
}

#[derive(Clone, Debug, Default)]
pub struct RepoSymbolIndex {
    symbols: HashMap<String, Vec<SymbolHit>>,
    pub file_count: usize,
    pub symbol_count: usize,
    pub build_ms: f64,
    pub refreshed_files: usize,
    pub disk_loaded: bool,
}

#[derive(Clone)]
struct CachedIndex {
    built_at: Instant,
    index: RepoSymbolIndex,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CachedLookupResult {
    pub cached: bool,
    pub disk_loaded: bool,
    pub refreshed_files: usize,
    pub file_count: usize,
    pub symbol_count: usize,
    pub build_ms: f64,
    pub lookup_ms: f64,
    pub hits: Vec<(String, Vec<SymbolHit>)>,
}

fn cache() -> &'static Mutex<HashMap<PathBuf, CachedIndex>> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, CachedIndex>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn invalidate_changed_path(workspace_root: &str, changed_path: &str) {
    let raw = Path::new(changed_path);
    let candidate = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        Path::new(workspace_root).join(raw)
    };
    let candidate = candidate.canonicalize().unwrap_or(candidate);
    cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|root, _| !candidate.starts_with(root));
}

pub fn lookup_cached(
    root: &Path,
    names: &[String],
    refresh: bool,
) -> Result<CachedLookupResult, String> {
    let root = root
        .canonicalize()
        .map_err(|e| format!("invalid repository root: {e}"))?;

    if !refresh {
        let guard = cache().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = guard.get(&root)
            && entry.built_at.elapsed() <= MEMORY_TTL
        {
            let started = Instant::now();
            let hits = entry.index.lookup(names);
            return Ok(CachedLookupResult {
                cached: true,
                disk_loaded: entry.index.disk_loaded,
                refreshed_files: entry.index.refreshed_files,
                file_count: entry.index.file_count,
                symbol_count: entry.index.symbol_count,
                build_ms: entry.index.build_ms,
                lookup_ms: started.elapsed().as_secs_f64() * 1000.0,
                hits,
            });
        }
    }

    let index = RepoSymbolIndex::load_or_refresh(&root, refresh)?;
    let started = Instant::now();
    let hits = index.lookup(names);
    let lookup_ms = started.elapsed().as_secs_f64() * 1000.0;
    let result = CachedLookupResult {
        cached: false,
        disk_loaded: index.disk_loaded,
        refreshed_files: index.refreshed_files,
        file_count: index.file_count,
        symbol_count: index.symbol_count,
        build_ms: index.build_ms,
        lookup_ms,
        hits,
    };
    cache().lock().unwrap_or_else(|e| e.into_inner()).insert(
        root,
        CachedIndex {
            built_at: Instant::now(),
            index,
        },
    );
    Ok(result)
}

impl RepoSymbolIndex {
    fn load_or_refresh(root: &Path, force_full: bool) -> Result<Self, String> {
        let started = Instant::now();
        let files = git_source_files(root)?;
        let disk_path = disk_cache_path(root)?;
        let mut disk_loaded = false;
        let mut previous = if !force_full {
            match load_disk_index(&disk_path, root) {
                Ok(Some(index)) => {
                    disk_loaded = true;
                    index
                }
                Ok(None) => empty_disk_index(root),
                Err(_) => empty_disk_index(root),
            }
        } else {
            empty_disk_index(root)
        };

        let patterns = compile_patterns()?;
        let current_paths = files
            .iter()
            .map(|path| normalize_rel(path))
            .collect::<HashSet<_>>();
        previous
            .files
            .retain(|path, _| current_paths.contains(path));

        let mut refreshed_files = 0usize;
        for rel in files {
            let rel_key = normalize_rel(&rel);
            let full = root.join(&rel);
            let Ok(fingerprint) = fingerprint(&full) else {
                previous.files.remove(&rel_key);
                continue;
            };

            let unchanged = previous
                .files
                .get(&rel_key)
                .is_some_and(|entry| same_fingerprint(&entry.fingerprint, &fingerprint));
            if unchanged {
                continue;
            }

            let Some(symbols) = parse_file_symbols(&full, &rel_key, &patterns) else {
                previous.files.remove(&rel_key);
                continue;
            };
            previous.files.insert(
                rel_key,
                FileEntry {
                    fingerprint,
                    symbols,
                },
            );
            refreshed_files += 1;
        }

        previous.version = INDEX_FORMAT_VERSION;
        previous.root = root.to_string_lossy().into_owned();
        save_disk_index(&disk_path, &previous)?;

        let mut index = from_disk_index(previous);
        index.build_ms = started.elapsed().as_secs_f64() * 1000.0;
        index.refreshed_files = refreshed_files;
        index.disk_loaded = disk_loaded;
        Ok(index)
    }

    pub fn lookup(&self, names: &[String]) -> Vec<(String, Vec<SymbolHit>)> {
        names
            .iter()
            .map(|name| {
                (
                    name.clone(),
                    self.symbols.get(name).cloned().unwrap_or_default(),
                )
            })
            .collect()
    }
}

fn git_source_files(root: &Path) -> Result<Vec<PathBuf>, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["ls-files", "-co", "--exclude-standard", "-z"])
        .output()
        .map_err(|e| format!("git ls-files failed: {e}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).into_owned());
    }
    Ok(output
        .stdout
        .split(|b| *b == 0)
        .filter(|item| !item.is_empty())
        .filter_map(|item| std::str::from_utf8(item).ok())
        .map(PathBuf::from)
        .filter(|p| is_source(p))
        .collect())
}

fn parse_file_symbols(
    full: &Path,
    rel_key: &str,
    patterns: &[(&'static str, Regex)],
) -> Option<Vec<(String, SymbolHit)>> {
    let text = fs::read_to_string(full).ok()?;
    let lines = text.lines().collect::<Vec<_>>();
    let mut symbols = Vec::new();
    for (idx, line) in lines.iter().enumerate() {
        for (kind, regex) in patterns {
            let Some(caps) = regex.captures(line) else {
                continue;
            };
            let Some(name) = caps.get(1).map(|v| v.as_str()) else {
                continue;
            };
            let start = idx.saturating_sub(2);
            let end = (idx + 3).min(lines.len());
            symbols.push((
                name.to_string(),
                SymbolHit {
                    kind: (*kind).to_string(),
                    path: rel_key.to_string(),
                    line: idx + 1,
                    context: lines[start..end].join("\n"),
                },
            ));
            break;
        }
    }
    Some(symbols)
}

fn from_disk_index(disk: DiskIndex) -> RepoSymbolIndex {
    let mut symbols: HashMap<String, Vec<SymbolHit>> = HashMap::new();
    let mut symbol_count = 0usize;
    for entry in disk.files.values() {
        for (name, hit) in &entry.symbols {
            symbols.entry(name.clone()).or_default().push(hit.clone());
            symbol_count += 1;
        }
    }
    RepoSymbolIndex {
        symbols,
        file_count: disk.files.len(),
        symbol_count,
        ..Default::default()
    }
}

fn empty_disk_index(root: &Path) -> DiskIndex {
    DiskIndex {
        version: INDEX_FORMAT_VERSION,
        root: root.to_string_lossy().into_owned(),
        files: HashMap::new(),
    }
}

fn load_disk_index(path: &Path, root: &Path) -> Result<Option<DiskIndex>, String> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("read symbol index cache failed: {error}")),
    };
    let index: DiskIndex = serde_json::from_slice(&bytes)
        .map_err(|e| format!("decode symbol index cache failed: {e}"))?;
    if index.version != INDEX_FORMAT_VERSION || index.root != root.to_string_lossy() {
        return Ok(None);
    }
    Ok(Some(index))
}

fn save_disk_index(path: &Path, index: &DiskIndex) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "symbol index cache path has no parent".to_string())?;
    fs::create_dir_all(parent).map_err(|e| format!("create symbol index cache dir failed: {e}"))?;
    let bytes =
        serde_json::to_vec(index).map_err(|e| format!("encode symbol index cache failed: {e}"))?;
    let temp = path.with_extension("json.tmp");
    fs::write(&temp, bytes).map_err(|e| format!("write symbol index cache failed: {e}"))?;
    fs::rename(&temp, path).map_err(|e| format!("replace symbol index cache failed: {e}"))?;
    Ok(())
}

fn disk_cache_path(root: &Path) -> Result<PathBuf, String> {
    let home =
        crate::state::user_home_dir().map_err(|e| format!("resolve CatDesk home failed: {e}"))?;
    let mut hasher = Sha256::new();
    hasher.update(root.to_string_lossy().as_bytes());
    hasher.update(INDEX_FORMAT_VERSION.to_le_bytes());
    let digest = hasher.finalize();
    let key = digest
        .iter()
        .take(16)
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    Ok(home
        .join(".catdesk")
        .join("cache")
        .join("symbol-index")
        .join(format!("{key}.json")))
}

fn fingerprint(path: &Path) -> Result<FileFingerprint, String> {
    let metadata =
        fs::metadata(path).map_err(|e| format!("stat {} failed: {e}", path.display()))?;
    let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
    let modified_ns = modified
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    Ok(FileFingerprint {
        size: metadata.len(),
        modified_ns,
    })
}

fn same_fingerprint(a: &FileFingerprint, b: &FileFingerprint) -> bool {
    a.size == b.size && a.modified_ns == b.modified_ns
}

fn normalize_rel(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn is_source(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|v| v.to_str())
            .map(|v| v.to_ascii_lowercase())
            .as_deref(),
        Some("rs" | "ts" | "tsx" | "js" | "jsx" | "mjs" | "py")
    )
}

fn compile_patterns() -> Result<Vec<(&'static str, Regex)>, String> {
    [
        (
            "function",
            r"^\s*(?:export\s+)?(?:async\s+)?function\s+([A-Za-z_$][A-Za-z0-9_$]*)",
        ),
        (
            "class",
            r"^\s*(?:export\s+)?(?:default\s+)?class\s+([A-Za-z_$][A-Za-z0-9_$]*)",
        ),
        (
            "interface",
            r"^\s*(?:export\s+)?interface\s+([A-Za-z_$][A-Za-z0-9_$]*)",
        ),
        (
            "type",
            r"^\s*(?:export\s+)?type\s+([A-Za-z_$][A-Za-z0-9_$]*)\s*=",
        ),
        (
            "enum",
            r"^\s*(?:export\s+)?enum\s+([A-Za-z_$][A-Za-z0-9_$]*)",
        ),
        (
            "const",
            r"^\s*(?:export\s+)?const\s+([A-Za-z_$][A-Za-z0-9_$]*)\s*=",
        ),
        (
            "let",
            r"^\s*(?:export\s+)?let\s+([A-Za-z_$][A-Za-z0-9_$]*)\s*=",
        ),
        (
            "var",
            r"^\s*(?:export\s+)?var\s+([A-Za-z_$][A-Za-z0-9_$]*)\s*=",
        ),
        (
            "pyfn",
            r"^\s*(?:async\s+)?def\s+([A-Za-z_][A-Za-z0-9_]*)\s*\(",
        ),
        ("pyclass", r"^\s*class\s+([A-Za-z_][A-Za-z0-9_]*)\s*[:(]"),
        (
            "rustfn",
            r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?fn\s+([A-Za-z_][A-Za-z0-9_]*)",
        ),
        (
            "ruststruct",
            r"^\s*(?:pub(?:\([^)]*\))?\s+)?struct\s+([A-Za-z_][A-Za-z0-9_]*)",
        ),
        (
            "rustenum",
            r"^\s*(?:pub(?:\([^)]*\))?\s+)?enum\s+([A-Za-z_][A-Za-z0-9_]*)",
        ),
        (
            "rusttrait",
            r"^\s*(?:pub(?:\([^)]*\))?\s+)?trait\s+([A-Za-z_][A-Za-z0-9_]*)",
        ),
    ]
    .into_iter()
    .map(|(kind, pattern)| {
        Regex::new(pattern)
            .map(|r| (kind, r))
            .map_err(|e| e.to_string())
    })
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_repo(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "catdesk-repo-index-{label}-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(root.join("src")).expect("create temp repo");
        Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(["init", "-q"])
            .status()
            .expect("git init");
        root
    }

    fn clear_memory_cache() {
        cache().lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    #[test]
    fn persists_index_and_reuses_it_after_memory_cache_clear() {
        let root = temp_repo("persist");
        fs::write(
            root.join("src/sample.ts"),
            "export function usefulThing() {\n  return 42;\n}\n",
        )
        .expect("write sample");
        let names = vec!["usefulThing".to_string()];
        let first = lookup_cached(&root, &names, true).expect("first lookup");
        assert!(!first.disk_loaded);
        assert_eq!(first.refreshed_files, 1);
        clear_memory_cache();
        let second = lookup_cached(&root, &names, false).expect("disk lookup");
        assert!(second.disk_loaded);
        assert_eq!(second.refreshed_files, 0);
        assert!(second.hits[0].1[0].context.contains("return 42"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn refreshes_only_changed_file() {
        let root = temp_repo("incremental");
        fs::write(
            root.join("src/a.ts"),
            "export function alphaThing() { return 1; }\n",
        )
        .expect("write a");
        fs::write(
            root.join("src/b.ts"),
            "export function betaThing() { return 2; }\n",
        )
        .expect("write b");
        let names = vec!["alphaThing".to_string(), "betaThing".to_string()];
        let first = lookup_cached(&root, &names, true).expect("first lookup");
        assert_eq!(first.refreshed_files, 2);
        clear_memory_cache();
        std::thread::sleep(Duration::from_millis(5));
        fs::write(
            root.join("src/a.ts"),
            "export function alphaThing() { return 3; }\n",
        )
        .expect("rewrite a");
        let second = lookup_cached(&root, &names, false).expect("incremental lookup");
        assert!(second.disk_loaded);
        assert_eq!(second.refreshed_files, 1);
        assert!(second.hits[0].1[0].context.contains("return 3"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn drops_deleted_file_from_persisted_index() {
        let root = temp_repo("delete");
        let file = root.join("src/a.ts");
        fs::write(&file, "export function goneThing() { return 1; }\n").expect("write a");
        let names = vec!["goneThing".to_string()];
        let _ = lookup_cached(&root, &names, true).expect("first lookup");
        clear_memory_cache();
        fs::remove_file(&file).expect("remove a");
        let second = lookup_cached(&root, &names, false).expect("refresh after delete");
        assert!(second.disk_loaded);
        assert!(second.hits[0].1.is_empty());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn changed_path_invalidates_matching_memory_cache() {
        let root = temp_repo("invalidate");
        let file = root.join("src/a.ts");
        fs::write(&file, "export function alphaThing() { return 1; }\n").expect("write a");
        let names = vec!["alphaThing".to_string()];
        let _ = lookup_cached(&root, &names, true).expect("first lookup");
        let root_str = root.to_string_lossy().into_owned();
        invalidate_changed_path(&root_str, "src/a.ts");
        std::thread::sleep(Duration::from_millis(5));
        fs::write(&file, "export function alphaThing() { return 9; }\n").expect("rewrite a");
        let second = lookup_cached(&root, &names, false).expect("lookup after invalidate");
        assert!(!second.cached);
        assert_eq!(second.refreshed_files, 1);
        assert!(second.hits[0].1[0].context.contains("return 9"));
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    #[ignore]
    async fn benchmark_daemon_persistent_restart() {
        let root = std::env::var("CATDESK_BENCH_REPO").expect("CATDESK_BENCH_REPO");
        let names = [
            "titleIndex",
            "sourcePath",
            "transcribe_chunk",
            "currentBatchId",
            "baselineNextStep",
            "tokenUsage",
            "baselineByKey",
            "roleValue",
        ]
        .into_iter()
        .map(str::to_string)
        .collect::<Vec<_>>();
        let root_path = Path::new(&root);

        clear_memory_cache();
        let first_started = Instant::now();
        let first = lookup_cached(root_path, &names, true).expect("first build");
        let first_wall_ms = first_started.elapsed().as_secs_f64() * 1000.0;

        clear_memory_cache();
        let restart_started = Instant::now();
        let restart = lookup_cached(root_path, &names, false).expect("restart lookup");
        let restart_wall_ms = restart_started.elapsed().as_secs_f64() * 1000.0;

        println!(
            "PERSIST_BENCH first_wall_ms={:.3} first_build_ms={:.3} restart_wall_ms={:.3} restart_build_ms={:.3} restart_refreshed={} disk_loaded={}",
            first_wall_ms,
            first.build_ms,
            restart_wall_ms,
            restart.build_ms,
            restart.refreshed_files,
            restart.disk_loaded
        );
    }
}
