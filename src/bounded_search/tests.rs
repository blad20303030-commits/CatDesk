use super::*;
use serde_json::Value;
use std::path::{Path, PathBuf};
use uuid::Uuid;

fn temp_root(name: &str) -> PathBuf {
    let root =
        std::env::temp_dir().join(format!("catdesk-bounded-search-{name}-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    root
}

fn request(pattern: &str) -> Request {
    Request {
        pattern: pattern.into(),
        ..Default::default()
    }
}

fn run(root: &Path, r: &Request) -> Value {
    r.validate().unwrap();
    let start = Instant::now();
    scan(
        root.to_str().unwrap(),
        r,
        start,
        Arc::new(AtomicBool::new(false)),
        Arc::new(Mutex::new(Output::new())),
    )
    .unwrap()
    .finish(r, start, true)
}

fn reason(v: &Value, needle: &str) -> bool {
    v["searchDiagnostics"]["stopReasons"]
        .as_array()
        .is_some_and(|values| values.iter().any(|value| value == needle))
}

#[test]
fn generated_directories_are_skipped_by_default_but_explicit_roots_work() {
    let root = temp_root("generated");
    std::fs::create_dir(root.join("node_modules")).unwrap();
    std::fs::write(root.join("node_modules").join("a.txt"), "TOKEN\n").unwrap();
    std::fs::write(root.join("source.txt"), "TOKEN\n").unwrap();

    let broad = run(&root, &request("TOKEN"));
    assert_eq!(broad["matchCount"], 1);
    assert_eq!(broad["searchScope"]["sourceOnly"], true);

    let explicit = run(
        &root,
        &Request {
            path: Some("node_modules".into()),
            ..request("TOKEN")
        },
    );
    assert_eq!(explicit["matchCount"], 1);

    let all = run(
        &root,
        &Request {
            source_only: false,
            ..request("TOKEN")
        },
    );
    assert_eq!(all["matchCount"], 2);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn catdeskignore_and_no_ignore_are_respected() {
    let root = temp_root("ignore");
    std::fs::write(root.join(".catdeskignore"), "ignored.txt\n").unwrap();
    std::fs::write(root.join("visible.txt"), "TOKEN\n").unwrap();
    std::fs::write(root.join("ignored.txt"), "TOKEN\n").unwrap();

    assert_eq!(run(&root, &request("TOKEN"))["matchCount"], 1);
    let all = run(
        &root,
        &Request {
            no_ignore: true,
            ..request("TOKEN")
        },
    );
    assert_eq!(all["matchCount"], 2);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn byte_budget_returns_bounded_partial_result() {
    let root = temp_root("budget");
    std::fs::write(root.join("large.txt"), "TOKEN\n".repeat(10_000)).unwrap();
    let value = run(
        &root,
        &Request {
            max_bytes: 128,
            ..request("TOKEN")
        },
    );
    assert!(value["searchTruncated"].as_bool().unwrap());
    assert!(reason(&value, "byte_budget"));
    assert!(value["searchDiagnostics"]["bytesRead"].as_u64().unwrap() <= 128);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn invalid_budgets_and_patterns_fail_validation() {
    assert!(
        Request {
            timeout_ms: 0,
            ..request("TOKEN")
        }
        .validate()
        .is_err()
    );
    assert!(request("[").matcher().is_err());
}

#[tokio::test]
async fn timeout_is_bounded_and_does_not_create_unbounded_queue() {
    let root = temp_root("timeout");
    std::fs::write(root.join("a.txt"), "TOKEN\n".repeat(1000)).unwrap();
    let req = Request {
        timeout_ms: 10,
        test_delay_ms: 100,
        ..request("TOKEN")
    };
    let value = search_with(
        root.to_string_lossy().into_owned(),
        req,
        Arc::new(Semaphore::new(1)),
    )
    .await
    .unwrap();
    assert!(reason(&value, "time_budget"));
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn git_prefilter_finds_tracked_and_untracked_non_ignored_files() {
    let root = temp_root("git-prefilter");
    let status = std::process::Command::new("git")
        .arg("init")
        .arg("-q")
        .arg(&root)
        .status();
    if !status.is_ok_and(|status| status.success()) {
        let _ = std::fs::remove_dir_all(root);
        return;
    }

    std::fs::write(root.join("tracked.txt"), "TOKEN tracked\n").unwrap();
    std::fs::write(root.join("untracked.txt"), "TOKEN untracked\n").unwrap();
    std::fs::write(root.join("ignored.txt"), "TOKEN ignored\n").unwrap();
    std::fs::write(root.join(".gitignore"), "ignored.txt\n").unwrap();

    assert!(
        std::process::Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(["add", "tracked.txt", ".gitignore"])
            .status()
            .unwrap()
            .success()
    );

    let value = run(
        &root,
        &Request {
            fixed_strings: true,
            ..request("TOKEN")
        },
    );
    assert_eq!(value["searchBackend"], "git-prefilter+rust-stream");
    assert_eq!(value["matchCount"], 2);

    let paths = value["searchResults"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|entry| entry["isContext"] == false)
        .filter_map(|entry| entry["path"].as_str())
        .collect::<Vec<_>>();
    assert!(paths.iter().any(|path| path.ends_with("tracked.txt")));
    assert!(paths.iter().any(|path| path.ends_with("untracked.txt")));
    assert!(!paths.iter().any(|path| path.ends_with("ignored.txt")));

    let _ = std::fs::remove_dir_all(root);
}
