use crate::command;
use sha2::{Digest, Sha256};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use uuid::Uuid;

pub const MAX_PATCH_CHUNK_BYTES: usize = 128 * 1024;
pub const MAX_PATCH_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct PatchSession {
    pub id: String,
    pub path: PathBuf,
    pub size_bytes: u64,
}

#[derive(Clone, Debug)]
pub struct PatchApplyResult {
    pub id: String,
    pub path: String,
    pub size_bytes: u64,
    pub sha256: String,
    pub check_stdout: String,
    pub apply_stdout: String,
}

fn canonical_workspace_root(workspace_root: &str) -> Result<PathBuf, String> {
    Path::new(workspace_root)
        .canonicalize()
        .map(command::normalize_windows_verbatim_path)
        .map_err(|error| error.to_string())
}

fn patches_dir(workspace_root: &str) -> Result<PathBuf, String> {
    Ok(canonical_workspace_root(workspace_root)?
        .join(".catdesk")
        .join("patches"))
}

fn validate_session_id(id: &str) -> Result<(), String> {
    Uuid::parse_str(id)
        .map(|_| ())
        .map_err(|_| "Invalid patch session id".to_string())
}

fn session_path(workspace_root: &str, id: &str) -> Result<PathBuf, String> {
    validate_session_id(id)?;
    Ok(patches_dir(workspace_root)?.join(format!("{id}.patch")))
}

pub fn begin(workspace_root: &str) -> Result<PatchSession, String> {
    let dir = patches_dir(workspace_root)?;
    fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
    let id = Uuid::new_v4().to_string();
    let path = dir.join(format!("{id}.patch"));
    OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&path)
        .map_err(|error| error.to_string())?;
    Ok(PatchSession {
        id,
        path,
        size_bytes: 0,
    })
}

pub fn append_chunk(
    workspace_root: &str,
    id: &str,
    expected_offset: u64,
    data: &str,
) -> Result<PatchSession, String> {
    if data.is_empty() {
        return Err("Patch chunk must not be empty".into());
    }
    if data.len() > MAX_PATCH_CHUNK_BYTES {
        return Err(format!(
            "Patch chunk too large: {} bytes (max {})",
            data.len(),
            MAX_PATCH_CHUNK_BYTES
        ));
    }

    let path = session_path(workspace_root, id)?;
    let current_size = fs::metadata(&path)
        .map_err(|error| format!("Patch session not found: {error}"))?
        .len();
    if current_size != expected_offset {
        return Err(format!(
            "Patch offset mismatch: expected {expected_offset}, actual {current_size}"
        ));
    }
    let new_size = current_size.saturating_add(data.len() as u64);
    if new_size > MAX_PATCH_BYTES {
        return Err(format!(
            "Patch exceeds maximum size: {new_size} bytes (max {MAX_PATCH_BYTES})"
        ));
    }

    let mut file = OpenOptions::new()
        .append(true)
        .open(&path)
        .map_err(|error| error.to_string())?;
    file.write_all(data.as_bytes())
        .map_err(|error| error.to_string())?;
    file.flush().map_err(|error| error.to_string())?;

    Ok(PatchSession {
        id: id.to_string(),
        path,
        size_bytes: new_size,
    })
}

pub fn abort(workspace_root: &str, id: &str) -> Result<(), String> {
    let path = session_path(workspace_root, id)?;
    if path.exists() {
        fs::remove_file(path).map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn sha256_file(path: &Path) -> Result<String, String> {
    let data = fs::read(path).map_err(|error| error.to_string())?;
    let mut hasher = Sha256::new();
    hasher.update(data);
    Ok(format!("{:x}", hasher.finalize()))
}

fn git_apply(workspace_root: &Path, patch_path: &Path, check: bool) -> Result<String, String> {
    let mut command = Command::new("git");
    command.current_dir(workspace_root).arg("apply");
    if check {
        command.arg("--check");
    }
    command.arg("--whitespace=nowarn").arg(patch_path);

    let output = command.output().map_err(|error| error.to_string())?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        return Err(format!(
            "git apply{} failed (exit {}): {}{}",
            if check { " --check" } else { "" },
            output.status.code().unwrap_or(-1),
            stdout,
            stderr
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

pub fn apply(
    workspace_root_str: &str,
    id: &str,
    expected_size: u64,
    expected_sha256: &str,
    keep_patch: bool,
) -> Result<PatchApplyResult, String> {
    let workspace_root = canonical_workspace_root(workspace_root_str)?;
    let path = session_path(workspace_root_str, id)?;
    let actual_size = fs::metadata(&path)
        .map_err(|error| format!("Patch session not found: {error}"))?
        .len();

    if actual_size != expected_size {
        return Err(format!(
            "Patch size mismatch: expected {expected_size}, actual {actual_size}"
        ));
    }
    let actual_sha256 = sha256_file(&path)?;
    if !actual_sha256.eq_ignore_ascii_case(expected_sha256) {
        return Err(format!(
            "Patch SHA-256 mismatch: expected {}, actual {}",
            expected_sha256, actual_sha256
        ));
    }

    let check_stdout = git_apply(&workspace_root, &path, true)?;
    let apply_stdout = git_apply(&workspace_root, &path, false)?;

    let relative_path = path
        .strip_prefix(&workspace_root)
        .unwrap_or(path.as_path())
        .display()
        .to_string()
        .replace('\\', "/");

    if !keep_patch {
        fs::remove_file(&path).map_err(|error| error.to_string())?;
    }

    Ok(PatchApplyResult {
        id: id.to_string(),
        path: relative_path,
        size_bytes: actual_size,
        sha256: actual_sha256,
        check_stdout,
        apply_stdout,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("catdesk-patch-{name}-{}", Uuid::new_v4()))
    }

    fn init_git(root: &Path) {
        fs::create_dir_all(root).expect("create workspace");
        let status = Command::new("git")
            .current_dir(root)
            .args(["init", "-q"])
            .status()
            .expect("run git init");
        assert!(status.success());
    }

    #[test]
    fn chunks_are_offset_guarded_and_size_limited() {
        let root = workspace("chunks");
        init_git(&root);
        let root_str = root.to_string_lossy().into_owned();
        let session = begin(&root_str).expect("begin");
        let session = append_chunk(&root_str, &session.id, 0, "abc").expect("first chunk");
        assert_eq!(session.size_bytes, 3);
        let error = append_chunk(&root_str, &session.id, 0, "def").unwrap_err();
        assert!(error.contains("offset mismatch"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn apply_checks_hash_before_touching_workspace() {
        let root = workspace("hash");
        init_git(&root);
        fs::write(root.join("note.txt"), "before\n").expect("seed file");
        let root_str = root.to_string_lossy().into_owned();
        let session = begin(&root_str).expect("begin");
        let patch = "--- a/note.txt\n+++ b/note.txt\n@@ -1 +1 @@\n-before\n+after\n";
        let session = append_chunk(&root_str, &session.id, 0, patch).expect("append");
        let error = apply(
            &root_str,
            &session.id,
            session.size_bytes,
            "deadbeef",
            false,
        )
        .unwrap_err();
        assert!(error.contains("SHA-256 mismatch"));
        assert_eq!(
            fs::read_to_string(root.join("note.txt")).unwrap(),
            "before\n"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn valid_patch_is_checked_then_applied() {
        let root = workspace("apply");
        init_git(&root);
        fs::write(root.join("note.txt"), "before\n").expect("seed file");
        let root_str = root.to_string_lossy().into_owned();
        let session = begin(&root_str).expect("begin");
        let patch = "--- a/note.txt\n+++ b/note.txt\n@@ -1 +1 @@\n-before\n+after\n";
        let session = append_chunk(&root_str, &session.id, 0, patch).expect("append");
        let digest = sha256_file(&session.path).expect("hash");
        let result =
            apply(&root_str, &session.id, session.size_bytes, &digest, false).expect("apply");
        assert_eq!(result.size_bytes, patch.len() as u64);
        assert_eq!(
            fs::read_to_string(root.join("note.txt"))
                .unwrap()
                .replace("\r\n", "\n"),
            "after\n"
        );
        assert!(!session.path.exists());
        let _ = fs::remove_dir_all(root);
    }
}
