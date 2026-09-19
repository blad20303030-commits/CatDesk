use serde::Serialize;
use std::collections::HashSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

pub const DEFAULT_MAX_AGENTS: usize = 4;
pub const DEFAULT_MAX_SKILLS: usize = 8;
pub const MAX_ROUTE_AGENTS: usize = 12;
pub const MAX_ROUTE_SKILLS: usize = 24;
pub const MAX_LOAD_AGENTS: usize = 8;
pub const MAX_LOAD_SKILLS: usize = 24;
const MAX_SINGLE_ENTRY_BYTES: usize = 160 * 1024;
const MAX_BUNDLE_BYTES: usize = 640 * 1024;

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogEntry {
    pub kind: String,
    pub name: String,
    pub description: String,
    pub relative_path: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RankedEntry {
    #[serde(flatten)]
    pub entry: CatalogEntry,
    pub score: i64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogStatus {
    pub root: String,
    pub source: String,
    pub agent_count: usize,
    pub skill_count: usize,
    pub total_count: usize,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RouteResult {
    pub root: String,
    pub task: String,
    pub agents: Vec<RankedEntry>,
    pub skills: Vec<RankedEntry>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LoadedEntry {
    pub kind: String,
    pub name: String,
    pub description: String,
    pub relative_path: String,
    pub content: String,
    pub truncated: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LoadResult {
    pub root: String,
    pub agents: Vec<LoadedEntry>,
    pub skills: Vec<LoadedEntry>,
    pub total_bytes: usize,
    pub bundle_truncated: bool,
}

#[derive(Clone, Debug)]
struct CatalogLocation {
    root: PathBuf,
    source: String,
}

pub fn status(workspace_root: &str) -> Result<CatalogStatus, String> {
    let location = locate_catalog(workspace_root)?;
    let entries = scan_catalog(&location.root)?;
    let agent_count = entries.iter().filter(|entry| entry.kind == "agent").count();
    let skill_count = entries.iter().filter(|entry| entry.kind == "skill").count();

    Ok(CatalogStatus {
        root: location.root.to_string_lossy().into_owned(),
        source: location.source,
        agent_count,
        skill_count,
        total_count: entries.len(),
    })
}

pub fn route(
    workspace_root: &str,
    task: &str,
    max_agents: usize,
    max_skills: usize,
) -> Result<RouteResult, String> {
    let task = task.trim();
    if task.is_empty() {
        return Err("Parameter task must not be empty".into());
    }

    let location = locate_catalog(workspace_root)?;
    let entries = scan_catalog(&location.root)?;
    Ok(route_entries(
        &location.root,
        task,
        &entries,
        max_agents.clamp(1, MAX_ROUTE_AGENTS),
        max_skills.clamp(1, MAX_ROUTE_SKILLS),
    ))
}

pub fn load(
    workspace_root: &str,
    agent_names: &[String],
    skill_names: &[String],
) -> Result<LoadResult, String> {
    if agent_names.is_empty() && skill_names.is_empty() {
        return Err("Provide at least one agent or skill name".into());
    }
    if agent_names.len() > MAX_LOAD_AGENTS {
        return Err(format!("Too many agents: maximum is {MAX_LOAD_AGENTS}"));
    }
    if skill_names.len() > MAX_LOAD_SKILLS {
        return Err(format!("Too many skills: maximum is {MAX_LOAD_SKILLS}"));
    }

    let location = locate_catalog(workspace_root)?;
    let entries = scan_catalog(&location.root)?;
    let mut total_bytes = 0usize;
    let mut bundle_truncated = false;

    let agents = load_kind(
        &location.root,
        &entries,
        "agent",
        agent_names,
        &mut total_bytes,
        &mut bundle_truncated,
    )?;
    let skills = load_kind(
        &location.root,
        &entries,
        "skill",
        skill_names,
        &mut total_bytes,
        &mut bundle_truncated,
    )?;

    Ok(LoadResult {
        root: location.root.to_string_lossy().into_owned(),
        agents,
        skills,
        total_bytes,
        bundle_truncated,
    })
}

fn locate_catalog(workspace_root: &str) -> Result<CatalogLocation, String> {
    let mut candidates = Vec::<(PathBuf, String)>::new();

    if let Ok(value) = env::var("CATDESK_ECC_CATALOG") {
        let value = value.trim();
        if !value.is_empty() {
            candidates.push((PathBuf::from(value), "CATDESK_ECC_CATALOG".into()));
        }
    }

    let workspace = Path::new(workspace_root);
    candidates.push((
        workspace.join(".catdesk").join("ecc-catalog"),
        "workspace .catdesk".into(),
    ));
    candidates.push((workspace.join("ecc-catalog"), "workspace".into()));

    if let Ok(current_dir) = env::current_dir() {
        add_ancestor_candidates(&mut candidates, &current_dir, "current directory");
    }
    if let Ok(current_exe) = env::current_exe() {
        add_ancestor_candidates(&mut candidates, &current_exe, "CatDesk executable");
    }
    if let Some(home) = user_home_dir() {
        candidates.push((
            home.join(".catdesk").join("ecc-catalog"),
            "user home .catdesk".into(),
        ));
    }

    let mut seen = HashSet::new();
    for (candidate, source) in candidates {
        let key = candidate.to_string_lossy().to_lowercase();
        if !seen.insert(key) {
            continue;
        }
        if is_catalog_root(&candidate) {
            let root = candidate
                .canonicalize()
                .unwrap_or_else(|_| candidate.clone());
            return Ok(CatalogLocation { root, source });
        }
    }

    Err(
        "ECC catalog not found. Set CATDESK_ECC_CATALOG to the catalog root, or place ecc-catalog next to the CatDesk repository."
            .into(),
    )
}

fn add_ancestor_candidates(candidates: &mut Vec<(PathBuf, String)>, start: &Path, source: &str) {
    for ancestor in start.ancestors().take(8) {
        candidates.push((ancestor.join("ecc-catalog"), format!("{source} ancestor")));
    }
}

fn user_home_dir() -> Option<PathBuf> {
    env::var_os("USERPROFILE")
        .or_else(|| env::var_os("HOME"))
        .map(PathBuf::from)
}

fn is_catalog_root(path: &Path) -> bool {
    path.join("agents").is_dir() && path.join("skills").is_dir()
}

fn scan_catalog(root: &Path) -> Result<Vec<CatalogEntry>, String> {
    let mut entries = Vec::new();

    let agents_dir = root.join("agents");
    for item in fs::read_dir(&agents_dir)
        .map_err(|error| format!("Failed to read {}: {error}", agents_dir.display()))?
    {
        let item = item.map_err(|error| format!("Failed to read agent entry: {error}"))?;
        let path = item.path();
        let is_markdown = path
            .extension()
            .and_then(|value| value.to_str())
            .is_some_and(|value| value.eq_ignore_ascii_case("md"));
        if path.is_file() && is_markdown {
            entries.push(entry_from_file(root, &path, "agent")?);
        }
    }

    let skills_dir = root.join("skills");
    let mut skill_files = Vec::new();
    collect_skill_files(&skills_dir, &mut skill_files)?;
    for path in skill_files {
        entries.push(entry_from_file(root, &path, "skill")?);
    }

    entries.sort_by(|left, right| {
        left.kind
            .cmp(&right.kind)
            .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
    });
    Ok(entries)
}

fn collect_skill_files(dir: &Path, output: &mut Vec<PathBuf>) -> Result<(), String> {
    for item in
        fs::read_dir(dir).map_err(|error| format!("Failed to read {}: {error}", dir.display()))?
    {
        let item = item.map_err(|error| format!("Failed to read skill entry: {error}"))?;
        let path = item.path();
        if path.is_dir() {
            collect_skill_files(&path, output)?;
        } else if path
            .file_name()
            .and_then(|value| value.to_str())
            .is_some_and(|value| value.eq_ignore_ascii_case("SKILL.md"))
        {
            output.push(path);
        }
    }
    Ok(())
}

fn entry_from_file(root: &Path, path: &Path, kind: &str) -> Result<CatalogEntry, String> {
    let content = fs::read_to_string(path)
        .map_err(|error| format!("Failed to read {}: {error}", path.display()))?;
    let (frontmatter_name, description) = parse_frontmatter(&content);

    let fallback_name = if kind == "skill" {
        path.parent()
            .and_then(Path::file_name)
            .and_then(|value| value.to_str())
            .unwrap_or("unknown")
            .to_string()
    } else {
        path.file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or("unknown")
            .to_string()
    };

    let relative_path = path
        .strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace(char::from(92), "/");

    Ok(CatalogEntry {
        kind: kind.into(),
        name: frontmatter_name.unwrap_or(fallback_name),
        description: description.unwrap_or_default(),
        relative_path,
    })
}

fn parse_frontmatter(content: &str) -> (Option<String>, Option<String>) {
    let mut lines = content.lines();
    if lines.next().map(str::trim) != Some("---") {
        return (None, None);
    }

    let mut name = None;
    let mut description = None;
    for line in lines {
        let trimmed = line.trim();
        if trimmed == "---" {
            break;
        }
        if name.is_none()
            && let Some(value) = trimmed.strip_prefix("name:")
        {
            name = nonempty_yaml_scalar(value);
            continue;
        }
        if description.is_none()
            && let Some(value) = trimmed.strip_prefix("description:")
        {
            description = nonempty_yaml_scalar(value);
        }
    }
    (name, description)
}

fn nonempty_yaml_scalar(value: &str) -> Option<String> {
    let value = value.trim();
    let value = value
        .strip_prefix('"')
        .and_then(|inner| inner.strip_suffix('"'))
        .unwrap_or(value);
    (!value.is_empty()).then(|| value.to_string())
}

fn route_entries(
    root: &Path,
    task: &str,
    entries: &[CatalogEntry],
    max_agents: usize,
    max_skills: usize,
) -> RouteResult {
    let mut agents = rank_kind(entries, "agent", task);
    let mut skills = rank_kind(entries, "skill", task);
    agents.truncate(max_agents);
    skills.truncate(max_skills);

    RouteResult {
        root: root.to_string_lossy().into_owned(),
        task: task.to_string(),
        agents,
        skills,
    }
}

fn rank_kind(entries: &[CatalogEntry], kind: &str, task: &str) -> Vec<RankedEntry> {
    let mut ranked = entries
        .iter()
        .filter(|entry| entry.kind == kind)
        .map(|entry| RankedEntry {
            entry: entry.clone(),
            score: score_entry(task, entry),
        })
        .filter(|entry| entry.score > 0)
        .collect::<Vec<_>>();

    ranked.sort_by(|left, right| {
        right
            .score
            .cmp(&left.score)
            .then_with(|| left.entry.name.cmp(&right.entry.name))
    });
    ranked
}

fn score_entry(task: &str, entry: &CatalogEntry) -> i64 {
    let query = normalize(task);
    let name = normalize(&entry.name);
    let description = normalize(&entry.description);
    let path = normalize(&entry.relative_path);
    if query.is_empty() {
        return 0;
    }

    let mut score = 0i64;
    if name == query {
        score += 5_000;
    } else if name.contains(&query) {
        score += 1_500;
    }

    let mut seen = HashSet::new();
    for token in query.split_whitespace() {
        if token.len() < 2 || !seen.insert(token) {
            continue;
        }
        if name == token {
            score += 500;
        } else if name.contains(token) {
            score += 180;
        }
        if description.contains(token) {
            score += 45;
        }
        if path.contains(token) {
            score += 20;
        }
    }
    score
}

fn normalize(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut last_was_space = true;

    for ch in value.chars().flat_map(char::to_lowercase) {
        if ch.is_alphanumeric() {
            output.push(ch);
            last_was_space = false;
        } else if !last_was_space {
            output.push(' ');
            last_was_space = true;
        }
    }

    output.trim().to_string()
}

fn load_kind(
    root: &Path,
    entries: &[CatalogEntry],
    kind: &str,
    requested: &[String],
    total_bytes: &mut usize,
    bundle_truncated: &mut bool,
) -> Result<Vec<LoadedEntry>, String> {
    let mut loaded = Vec::new();
    let mut missing = Vec::new();
    let mut seen = HashSet::new();

    for requested_name in requested {
        let requested_name = requested_name.trim();
        if requested_name.is_empty() || !seen.insert(requested_name.to_lowercase()) {
            continue;
        }

        let Some(entry) = entries
            .iter()
            .find(|entry| entry.kind == kind && entry.name.eq_ignore_ascii_case(requested_name))
        else {
            missing.push(requested_name.to_string());
            continue;
        };

        if *total_bytes >= MAX_BUNDLE_BYTES {
            *bundle_truncated = true;
            break;
        }

        let path = root.join(&entry.relative_path);
        let content = fs::read_to_string(&path)
            .map_err(|error| format!("Failed to read {}: {error}", path.display()))?;
        let remaining = MAX_BUNDLE_BYTES - *total_bytes;
        let limit = remaining.min(MAX_SINGLE_ENTRY_BYTES);
        let (content, truncated) = truncate_utf8(content, limit);
        *total_bytes += content.len();
        if truncated {
            *bundle_truncated = true;
        }

        loaded.push(LoadedEntry {
            kind: entry.kind.clone(),
            name: entry.name.clone(),
            description: entry.description.clone(),
            relative_path: entry.relative_path.clone(),
            content,
            truncated,
        });
    }

    if !missing.is_empty() {
        return Err(format!("Unknown {kind} name(s): {}", missing.join(", ")));
    }

    Ok(loaded)
}

fn truncate_utf8(mut content: String, max_bytes: usize) -> (String, bool) {
    if content.len() <= max_bytes {
        return (content, false);
    }

    let mut end = max_bytes.min(content.len());
    while end > 0 && !content.is_char_boundary(end) {
        end -= 1;
    }
    content.truncate(end);
    (content, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn fixture() -> PathBuf {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let root =
            env::temp_dir().join(format!("catdesk-ecc-test-{}-{suffix}", std::process::id()));

        fs::create_dir_all(root.join("agents")).unwrap();
        fs::create_dir_all(root.join("skills").join("testing")).unwrap();
        fs::create_dir_all(root.join("skills").join("accessibility")).unwrap();

        fs::write(
            root.join("agents").join("code-reviewer.md"),
            "---\nname: code-reviewer\ndescription: Review code quality and security\n---\nReview carefully.\n",
        )
        .unwrap();
        fs::write(
            root.join("agents").join("architect.md"),
            "---\nname: architect\ndescription: Design software architecture and system boundaries\n---\nDesign carefully.\n",
        )
        .unwrap();
        fs::write(
            root.join("skills").join("testing").join("SKILL.md"),
            "---\nname: testing\ndescription: Run tests and validate regressions\n---\nTest everything.\n",
        )
        .unwrap();
        fs::write(
            root.join("skills").join("accessibility").join("SKILL.md"),
            "---\nname: accessibility\ndescription: Audit UI accessibility\n---\nUse WCAG.\n",
        )
        .unwrap();

        root
    }

    #[test]
    fn public_api_discovers_workspace_catalog() {
        let workspace = fixture();
        let catalog = workspace.join("ecc-catalog");
        fs::rename(workspace.join("agents"), catalog.join("agents")).unwrap_or_else(|_| {
            fs::create_dir_all(&catalog).unwrap();
            fs::rename(workspace.join("agents"), catalog.join("agents")).unwrap();
        });
        if !catalog.join("skills").exists() {
            fs::rename(workspace.join("skills"), catalog.join("skills")).unwrap();
        }
        let workspace_str = workspace.to_string_lossy().into_owned();

        let status = status(&workspace_str).unwrap();
        assert_eq!(status.agent_count, 2);
        assert_eq!(status.skill_count, 2);

        let routed = route(&workspace_str, "review code security", 2, 2).unwrap();
        assert_eq!(
            routed.agents.first().map(|entry| entry.entry.name.as_str()),
            Some("code-reviewer")
        );

        let loaded = load(&workspace_str, &["architect".into()], &["testing".into()]).unwrap();
        assert_eq!(loaded.agents.len(), 1);
        assert_eq!(loaded.skills.len(), 1);
        assert!(loaded.skills[0].content.contains("Test everything."));

        let _ = fs::remove_dir_all(workspace);
    }

    #[test]
    fn scans_agents_and_skills() {
        let root = fixture();
        let entries = scan_catalog(&root).unwrap();

        assert_eq!(
            entries.iter().filter(|entry| entry.kind == "agent").count(),
            2
        );
        assert_eq!(
            entries.iter().filter(|entry| entry.kind == "skill").count(),
            2
        );
        assert!(entries.iter().any(|entry| entry.name == "code-reviewer"));
        assert!(entries.iter().any(|entry| entry.name == "testing"));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn routes_by_name_and_description_without_a_model() {
        let root = fixture();
        let entries = scan_catalog(&root).unwrap();
        let routed = route_entries(&root, "review code security", &entries, 2, 2);

        assert_eq!(
            routed.agents.first().map(|entry| entry.entry.name.as_str()),
            Some("code-reviewer")
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn loads_exact_entries_and_preserves_markdown() {
        let root = fixture();
        let entries = scan_catalog(&root).unwrap();
        let mut bytes = 0;
        let mut truncated = false;

        let loaded = load_kind(
            &root,
            &entries,
            "skill",
            &["testing".into()],
            &mut bytes,
            &mut truncated,
        )
        .unwrap();

        assert_eq!(loaded.len(), 1);
        assert!(loaded[0].content.contains("Test everything."));
        assert!(!truncated);

        let _ = fs::remove_dir_all(root);
    }
}
