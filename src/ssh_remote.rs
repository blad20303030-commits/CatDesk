use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Instant;

use serde::Serialize;
use tokio::process::Command;
use tokio::time::{Duration, timeout};

const DEFAULT_CONNECT_TIMEOUT_SECS: u64 = 10;
const MAX_CAPTURE_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone)]
pub struct SshTarget {
    pub host: String,
    pub user: String,
    pub port: u16,
    pub identity_file: Option<PathBuf>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SshRunResult {
    pub stdout: String,
    pub stderr: String,
    pub success: bool,
    pub exit_code: Option<i32>,
    pub elapsed_ms: u64,
    pub timed_out: bool,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
}

fn truncate_text(bytes: Vec<u8>) -> (String, bool) {
    if bytes.len() <= MAX_CAPTURE_BYTES {
        return (String::from_utf8_lossy(&bytes).into_owned(), false);
    }
    (
        String::from_utf8_lossy(&bytes[..MAX_CAPTURE_BYTES]).into_owned(),
        true,
    )
}

fn validate_token(label: &str, value: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err(format!("{label} must not be empty"));
    }
    if value
        .chars()
        .any(|ch| ch.is_whitespace() || matches!(ch, '\0' | '\r' | '\n'))
    {
        return Err(format!("{label} contains unsupported whitespace"));
    }
    Ok(())
}

pub fn validate_target(target: &SshTarget) -> Result<(), String> {
    validate_token("host", &target.host)?;
    validate_token("user", &target.user)?;
    if target.port == 0 {
        return Err("port must be between 1 and 65535".into());
    }
    if let Some(identity_file) = target.identity_file.as_deref() {
        if !identity_file.is_file() {
            return Err(format!(
                "identity_file does not exist or is not a file: {}",
                identity_file.display()
            ));
        }
    }
    Ok(())
}

fn add_common_args(command: &mut Command, target: &SshTarget) {
    command
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("IdentitiesOnly=yes")
        .arg("-o")
        .arg("StrictHostKeyChecking=yes")
        .arg("-o")
        .arg(format!("ConnectTimeout={DEFAULT_CONNECT_TIMEOUT_SECS}"))
        .arg("-p")
        .arg(target.port.to_string());

    if let Some(identity_file) = target.identity_file.as_deref() {
        command.arg("-i").arg(identity_file);
    }
}

fn destination(target: &SshTarget) -> String {
    format!("{}@{}", target.user, target.host)
}

pub async fn exec(target: &SshTarget, remote_command: &str, timeout_ms: u64) -> SshRunResult {
    let started = Instant::now();

    if let Err(error) = validate_target(target) {
        return failed(started, error);
    }
    if remote_command.trim().is_empty() {
        return failed(started, "command must not be empty".into());
    }

    let mut command = Command::new("ssh");
    add_common_args(&mut command, target);
    command
        .arg(destination(target))
        .arg(remote_command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    run_process(command, timeout_ms, started).await
}

pub async fn upload(
    target: &SshTarget,
    local_path: &Path,
    remote_path: &str,
    recursive: bool,
    timeout_ms: u64,
) -> SshRunResult {
    let started = Instant::now();

    if let Err(error) = validate_target(target) {
        return failed(started, error);
    }
    if !local_path.exists() {
        return failed(
            started,
            format!("local_path does not exist: {}", local_path.display()),
        );
    }
    if remote_path.trim().is_empty() || remote_path.contains(['\r', '\n']) {
        return failed(started, "remote_path is invalid".into());
    }
    if local_path.is_dir() && !recursive {
        return failed(
            started,
            "local_path is a directory; set recursive=true".into(),
        );
    }

    let mut command = Command::new("scp");
    command
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("IdentitiesOnly=yes")
        .arg("-o")
        .arg("StrictHostKeyChecking=yes")
        .arg("-o")
        .arg(format!("ConnectTimeout={DEFAULT_CONNECT_TIMEOUT_SECS}"))
        .arg("-P")
        .arg(target.port.to_string());

    if let Some(identity_file) = target.identity_file.as_deref() {
        command.arg("-i").arg(identity_file);
    }
    if recursive {
        command.arg("-r");
    }

    command
        .arg(local_path)
        .arg(format!("{}:{}", destination(target), remote_path))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    run_process(command, timeout_ms, started).await
}

pub async fn download(
    target: &SshTarget,
    remote_path: &str,
    local_path: &Path,
    recursive: bool,
    timeout_ms: u64,
) -> SshRunResult {
    let started = Instant::now();

    if let Err(error) = validate_target(target) {
        return failed(started, error);
    }
    if remote_path.trim().is_empty() || remote_path.contains(['\r', '\n']) {
        return failed(started, "remote_path is invalid".into());
    }

    let mut command = Command::new("scp");
    command
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("IdentitiesOnly=yes")
        .arg("-o")
        .arg("StrictHostKeyChecking=yes")
        .arg("-o")
        .arg(format!("ConnectTimeout={DEFAULT_CONNECT_TIMEOUT_SECS}"))
        .arg("-P")
        .arg(target.port.to_string());

    if let Some(identity_file) = target.identity_file.as_deref() {
        command.arg("-i").arg(identity_file);
    }
    if recursive {
        command.arg("-r");
    }

    command
        .arg(format!("{}:{}", destination(target), remote_path))
        .arg(local_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    run_process(command, timeout_ms, started).await
}

async fn run_process(mut command: Command, timeout_ms: u64, started: Instant) -> SshRunResult {
    let output = match timeout(Duration::from_millis(timeout_ms), command.output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => {
            return failed(started, format!("Failed to execute SSH process: {error}"));
        }
        Err(_) => {
            return SshRunResult {
                stdout: String::new(),
                stderr: format!("SSH operation timed out after {timeout_ms} ms"),
                success: false,
                exit_code: None,
                elapsed_ms: started.elapsed().as_millis() as u64,
                timed_out: true,
                stdout_truncated: false,
                stderr_truncated: false,
            };
        }
    };

    let (stdout, stdout_truncated) = truncate_text(output.stdout);
    let (stderr, stderr_truncated) = truncate_text(output.stderr);

    SshRunResult {
        stdout,
        stderr,
        success: output.status.success(),
        exit_code: output.status.code(),
        elapsed_ms: started.elapsed().as_millis() as u64,
        timed_out: false,
        stdout_truncated,
        stderr_truncated,
    }
}

fn failed(started: Instant, message: String) -> SshRunResult {
    SshRunResult {
        stdout: String::new(),
        stderr: message,
        success: false,
        exit_code: None,
        elapsed_ms: started.elapsed().as_millis() as u64,
        timed_out: false,
        stdout_truncated: false,
        stderr_truncated: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_validation_rejects_whitespace() {
        let target = SshTarget {
            host: "bad host".into(),
            user: "root".into(),
            port: 22,
            identity_file: None,
        };
        assert!(validate_target(&target).is_err());
    }

    #[test]
    fn target_validation_accepts_normal_host() {
        let target = SshTarget {
            host: "72.56.19.185".into(),
            user: "root".into(),
            port: 22,
            identity_file: None,
        };
        assert!(validate_target(&target).is_ok());
    }
}
