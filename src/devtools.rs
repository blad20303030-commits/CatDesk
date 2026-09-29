use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::browser::DetectedBrowser;
use crate::state::user_home_dir;

const DEVTOOLS_PROTOCOL_VERSION: &str = "2025-03-26";
const DEVTOOLS_CLIENT_NAME: &str = "catdesk-bridge";
const DEVTOOLS_CLIENT_VERSION: &str = "4.0.0";
const DEVTOOLS_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const DEVTOOLS_INTERACTIVE_TIMEOUT: Duration = Duration::from_secs(15);
const DEVTOOLS_SCREENSHOT_TIMEOUT: Duration = Duration::from_secs(20);
const DEVTOOLS_NAVIGATION_TIMEOUT: Duration = Duration::from_secs(35);
const RECOVERY_REQUEST_TIMEOUT: Duration = Duration::from_secs(8);
const RECOVERY_INTERVAL: Duration = Duration::from_secs(5);
const RECOVERY_RELOAD_WINDOW: Duration = Duration::from_secs(300);
const RECOVERY_COOLDOWN: Duration = Duration::from_secs(600);
const RECOVERY_MAX_RELOADS_PER_WINDOW: usize = 1;
const RECOVERY_REQUIRED_CONSECUTIVE_FAILURES: u8 = 2;
const RECOVERY_RESUME_TTL: Duration = Duration::from_secs(30 * 60);
const RECOVERY_RESUME_MESSAGE: &str = "CatDesk автоматически восстановил этот чат после сбоя страницы. Продолжай с места обрыва. Сначала сверь фактическое состояние проекта, файлов и процессов; не повторяй уже успешно выполненные команды или действия с побочными эффектами. Не нажимай «Повторить» и не запускай дубликаты — продолжи только незавершённую часть.";
const CHATGPT_HOME_URL: &str = "https://chatgpt.com/";

type PendingKey = (u64, Value);
type PendingRequests = Arc<Mutex<HashMap<PendingKey, tokio::sync::oneshot::Sender<Value>>>>;

static WATCHDOG_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Serialize, Deserialize)]
struct PendingResume {
    id: String,
    chat_url: String,
    user_turns: u64,
    assistant_turns: u64,
    last_assistant_chars: u64,
    created_unix_secs: u64,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct BrowserRecoveryState {
    last_chatgpt_url: Option<String>,
    pending_resume: Option<PendingResume>,
    last_resume_attempt_id: Option<String>,
}

pub struct UserCallGuard(Arc<std::sync::atomic::AtomicUsize>);
impl Drop for UserCallGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

#[derive(Clone, Default)]
pub struct PageMutationScheduler {
    locks: Arc<Mutex<HashMap<String, std::sync::Weak<Mutex<()>>>>>,
}

impl PageMutationScheduler {
    pub async fn lock_for(&self, key: &str) -> Arc<Mutex<()>> {
        let mut locks = self.locks.lock().await;
        locks.retain(|_, weak| weak.strong_count() > 0);
        if let Some(lock) = locks.get(key).and_then(std::sync::Weak::upgrade) {
            return lock;
        }
        let lock = Arc::new(Mutex::new(()));
        locks.insert(key.to_string(), Arc::downgrade(&lock));
        lock
    }
}

/// A running chrome-devtools-mcp child process with a short-held write lock.
/// Waiting for a JSON-RPC response never holds the outer DevtoolsBridge mutex.
pub struct DevtoolsBridge {
    #[allow(dead_code)]
    child: Child,
    stdin: tokio::io::BufWriter<tokio::process::ChildStdin>,
    pending: PendingRequests,
    launch_args: Vec<String>,
    generation: u64,
    tools_cache: Option<(u64, Vec<Value>)>,
    active_user_calls: Arc<std::sync::atomic::AtomicUsize>,
    page_scheduler: PageMutationScheduler,
    tools_refresh: Arc<Mutex<()>>,
}

impl DevtoolsBridge {
    pub async fn start(
        selected_browser: Option<&DetectedBrowser>,
    ) -> Result<Arc<Mutex<Self>>, String> {
        let launch_args = build_launch_args(selected_browser)?;
        let pending: PendingRequests = Arc::new(Mutex::new(HashMap::new()));
        let generation = 1;
        let (child, stdin) = spawn_devtools_process(&launch_args, pending.clone(), generation)?;

        let bridge = Arc::new(Mutex::new(Self {
            child,
            stdin,
            pending,
            launch_args,
            generation,
            tools_cache: None,
            active_user_calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            page_scheduler: PageMutationScheduler::default(),
            tools_refresh: Arc::new(Mutex::new(())),
        }));

        Self::initialize_transport_shared(&bridge).await?;
        spawn_chatgpt_recovery_watchdog(bridge.clone());
        Ok(bridge)
    }

    async fn initialize_transport_shared(bridge: &Arc<Mutex<Self>>) -> Result<(), String> {
        let init_req = json!({
            "jsonrpc": "2.0",
            "id": "dt-init",
            "method": "initialize",
            "params": {
                "protocolVersion": DEVTOOLS_PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {
                    "name": DEVTOOLS_CLIENT_NAME,
                    "version": DEVTOOLS_CLIENT_VERSION
                }
            }
        });
        Self::request_shared_with_timeout(bridge, &init_req, DEVTOOLS_REQUEST_TIMEOUT).await?;
        Self::notify_shared(
            bridge,
            &json!({
                "jsonrpc": "2.0",
                "method": "notifications/initialized"
            }),
        )
        .await
    }

    async fn send_locked(
        &mut self,
        req: &Value,
    ) -> Result<
        (
            Option<PendingKey>,
            Option<tokio::sync::oneshot::Receiver<Value>>,
            PendingRequests,
        ),
        String,
    > {
        let line = serde_json::to_string(req).map_err(|e| e.to_string())?;
        let id = req.get("id").cloned();
        let key = id.map(|id| (self.generation, id));
        let receiver = if let Some(key) = key.clone() {
            let (tx, rx) = tokio::sync::oneshot::channel();
            let mut pending = self.pending.lock().await;
            if pending.contains_key(&key) {
                return Err(
                    "Duplicate DevTools request id in the same transport generation".into(),
                );
            }
            pending.insert(key.clone(), tx);
            Some(rx)
        } else {
            None
        };
        let write = async {
            self.stdin
                .write_all(line.as_bytes())
                .await
                .map_err(|e| format!("stdin write: {e}"))?;
            self.stdin
                .write_all(b"\n")
                .await
                .map_err(|e| format!("stdin write newline: {e}"))?;
            self.stdin
                .flush()
                .await
                .map_err(|e| format!("stdin flush: {e}"))
        }
        .await;
        if let Err(error) = write {
            if let Some(key) = &key {
                self.pending.lock().await.remove(key);
            }
            return Err(error);
        }

        Ok((key, receiver, self.pending.clone()))
    }

    pub async fn request_shared(bridge: &Arc<Mutex<Self>>, req: &Value) -> Result<Value, String> {
        Self::request_shared_with_timeout(bridge, req, DEVTOOLS_REQUEST_TIMEOUT).await
    }

    pub fn user_tool_timeout(tool_name: &str) -> Duration {
        match tool_name {
            "take_screenshot" | "screenshot" => DEVTOOLS_SCREENSHOT_TIMEOUT,
            "navigate_page" | "new_page" => DEVTOOLS_NAVIGATION_TIMEOUT,
            "performance_start_trace"
            | "performance_stop_trace"
            | "lighthouse_audit"
            | "wait_for"
            | "upload_file" => DEVTOOLS_REQUEST_TIMEOUT,
            _ => DEVTOOLS_INTERACTIVE_TIMEOUT,
        }
    }

    pub async fn request_user_tool(
        bridge: &Arc<Mutex<Self>>,
        req: &Value,
        tool_name: &str,
    ) -> Result<Value, String> {
        Self::request_shared_with_timeout(bridge, req, Self::user_tool_timeout(tool_name)).await
    }

    async fn request_shared_with_timeout(
        bridge: &Arc<Mutex<Self>>,
        req: &Value,
        timeout: Duration,
    ) -> Result<Value, String> {
        let (key, receiver, pending) = {
            let mut guard = bridge.lock().await;
            guard.send_locked(req).await?
        };
        let Some(receiver) = receiver else {
            return Ok(Value::Null);
        };
        match tokio::time::timeout(timeout, receiver).await {
            Ok(Ok(response)) => Ok(response),
            Ok(Err(_)) => {
                if let Some(key) = &key {
                    pending.lock().await.remove(key);
                }
                Err("Response channel closed".into())
            }
            Err(_) => {
                if let Some(key) = &key {
                    pending.lock().await.remove(key);
                }
                Err(format!("Request timed out ({}s)", timeout.as_secs()))
            }
        }
    }

    pub async fn notify_shared(bridge: &Arc<Mutex<Self>>, req: &Value) -> Result<(), String> {
        let mut guard = bridge.lock().await;
        let _ = guard.send_locked(req).await?;
        Ok(())
    }

    pub async fn begin_user_call(bridge: &Arc<Mutex<Self>>) -> UserCallGuard {
        let activity = bridge.lock().await.active_user_calls.clone();
        activity.fetch_add(1, Ordering::AcqRel);
        UserCallGuard(activity)
    }

    async fn has_active_user_calls(bridge: &Arc<Mutex<Self>>) -> bool {
        let activity = bridge.lock().await.active_user_calls.clone();
        watchdog_should_defer(&activity)
    }

    pub async fn mutation_lock(bridge: &Arc<Mutex<Self>>, page_key: &str) -> Arc<Mutex<()>> {
        let scheduler = bridge.lock().await.page_scheduler.clone();
        scheduler.lock_for(page_key).await
    }

    pub async fn tools(bridge: &Arc<Mutex<Self>>) -> Result<Vec<Value>, String> {
        if let Some(tools) = cached_tools(bridge).await {
            return Ok(tools);
        }
        let refresh = bridge.lock().await.tools_refresh.clone();
        let _refresh_guard = refresh.lock().await;
        if let Some(tools) = cached_tools(bridge).await {
            return Ok(tools);
        }
        let generation = bridge.lock().await.generation;
        let response = Self::request_shared(
            bridge,
            &json!({
                "jsonrpc": "2.0",
                "id": format!("dt-tools-list-{generation}"),
                "method": "tools/list",
                "params": {}
            }),
        )
        .await?;
        let tools = response
            .pointer("/result/tools")
            .and_then(Value::as_array)
            .cloned()
            .ok_or("DevTools tools/list returned no tools")?;
        let mut guard = bridge.lock().await;
        if guard.generation == generation {
            guard.tools_cache = Some((generation, tools.clone()));
        }
        Ok(tools)
    }

    /// Restart only the DevTools transport. Never repeats the user's last tool call.
    async fn restart_transport_shared(bridge: &Arc<Mutex<Self>>) -> Result<(), String> {
        {
            let mut guard = bridge.lock().await;
            if guard.active_user_calls.load(Ordering::Acquire) > 0 {
                return Err("WATCHDOG_DEFERRED: user browser work became active".into());
            }
            let _ = guard.child.kill().await;
            guard.pending.lock().await.clear();
            guard.generation = guard.generation.saturating_add(1);
            guard.tools_cache = None;
            let generation = guard.generation;
            let (child, stdin) =
                spawn_devtools_process(&guard.launch_args, guard.pending.clone(), generation)?;
            guard.child = child;
            guard.stdin = stdin;
        }
        Self::initialize_transport_shared(bridge).await
    }

    #[allow(dead_code)]
    pub async fn stop_shared(bridge: &Arc<Mutex<Self>>) {
        let mut guard = bridge.lock().await;
        let _ = guard.child.kill().await;
        guard.pending.lock().await.clear();
        guard.generation = guard.generation.saturating_add(1);
        guard.tools_cache = None;
    }
}

async fn cached_tools(bridge: &Arc<Mutex<DevtoolsBridge>>) -> Option<Vec<Value>> {
    let guard = bridge.lock().await;
    guard
        .tools_cache
        .as_ref()
        .filter(|(generation, _)| *generation == guard.generation)
        .map(|(_, tools)| tools.clone())
}

fn watchdog_should_defer(activity: &std::sync::atomic::AtomicUsize) -> bool {
    activity.load(Ordering::Acquire) > 0
}

async fn route_response(pending: &PendingRequests, generation: u64, message: Value) {
    let Some(id) = message.get("id").cloned() else {
        return;
    };
    let sender = pending.lock().await.remove(&(generation, id));
    if let Some(sender) = sender {
        let _ = sender.send(message);
    }
}

async fn fail_generation(pending: &PendingRequests, generation: u64) {
    let mut map = pending.lock().await;
    map.retain(|(entry_generation, _), _| *entry_generation != generation);
}

fn build_launch_args(selected_browser: Option<&DetectedBrowser>) -> Result<Vec<String>, String> {
    let mut args = vec![
        "-y".to_string(),
        "chrome-devtools-mcp@1.10.1".to_string(),
        "--no-usage-statistics".to_string(),
        // The MCP binary keeps structured output off by default. Recovery must
        // consume stable machine-readable page data instead of parsing prose.
        "--experimentalStructuredContent".to_string(),
    ];

    if let Some(browser) = selected_browser {
        if cfg!(target_os = "windows") && browser.binary.starts_with("google-chrome") {
            // Chrome 144+ can expose the user's already-running browser session
            // through chrome://inspect/#remote-debugging. Prefer that session on
            // Windows so ChatGPT keeps the user's real profile, cookies, tabs,
            // and current conversation instead of a second managed profile.
            args.push("--autoConnect".into());
        } else if cfg!(target_os = "windows") {
            let target = browser
                .remote_debug_target
                .as_deref()
                .filter(|target| browser.remote_debug_active && *target != "pipe")
                .ok_or_else(|| {
                    format!(
                        "Windows browser control requires a running remote-debugging endpoint for {}",
                        browser.name
                    )
                })?;
            args.push("--browserUrl".into());
            args.push(format!("http://{target}"));
        } else if browser.remote_debug_active {
            if let Some(target) = browser.remote_debug_target.as_deref() {
                if target == "pipe" {
                    args.push("--executablePath".into());
                    args.push(browser.path.clone());
                    append_managed_profile_arg(&mut args, browser)?;
                } else {
                    args.push("--browserUrl".into());
                    args.push(format!("http://{target}"));
                }
            } else {
                args.push("--executablePath".into());
                args.push(browser.path.clone());
                append_managed_profile_arg(&mut args, browser)?;
            }
        } else {
            args.push("--executablePath".into());
            args.push(browser.path.clone());
            append_managed_profile_arg(&mut args, browser)?;
        }
    }

    Ok(args)
}

fn append_managed_profile_arg(
    args: &mut Vec<String>,
    browser: &DetectedBrowser,
) -> Result<(), String> {
    let profile_dir = managed_profile_dir(browser)?;
    std::fs::create_dir_all(&profile_dir).map_err(|e| {
        format!(
            "Failed to create browser profile {}: {e}",
            profile_dir.display()
        )
    })?;
    args.push("--userDataDir".into());
    args.push(profile_dir.to_string_lossy().into_owned());
    Ok(())
}

fn managed_profile_dir(browser: &DetectedBrowser) -> Result<PathBuf, String> {
    let browser_key = browser
        .binary
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();

    user_home_dir()
        .map(|home| {
            home.join(".catdesk")
                .join("browser-profiles")
                .join(browser_key)
        })
        .map_err(|e| format!("Failed to resolve CatDesk browser profile directory: {e}"))
}

fn npx_command() -> &'static str {
    if cfg!(target_os = "windows") {
        "npx.cmd"
    } else {
        "npx"
    }
}

fn spawn_devtools_process(
    launch_args: &[String],
    pending: PendingRequests,
    generation: u64,
) -> Result<(Child, tokio::io::BufWriter<tokio::process::ChildStdin>), String> {
    let mut command = Command::new(npx_command());
    command.args(launch_args);
    command
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());

    let mut child = command
        .spawn()
        .map_err(|e| format!("Failed to spawn chrome-devtools-mcp: {e}"))?;

    let child_stdin = child.stdin.take().ok_or("No stdin")?;
    let child_stdout = child.stdout.take().ok_or("No stdout")?;

    tokio::spawn(async move {
        let mut reader = BufReader::new(child_stdout);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) => break,
                Ok(_) => {
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    if let Ok(message) = serde_json::from_str::<Value>(trimmed) {
                        route_response(&pending, generation, message).await;
                    }
                }
                Err(_) => break,
            }
        }
        fail_generation(&pending, generation).await;
    });

    Ok((child, tokio::io::BufWriter::new(child_stdin)))
}

fn next_watchdog_id() -> String {
    format!(
        "catdesk-recovery-{}",
        WATCHDOG_REQUEST_ID.fetch_add(1, Ordering::Relaxed)
    )
}

fn watchdog_mutation_key(tool_name: &str, arguments: &Value) -> Option<String> {
    match tool_name {
        "navigate_page" | "type_text" => arguments
            .get("pageId")
            .or_else(|| arguments.get("page_id"))
            .map(|value| format!("page:{value}")),
        "new_page" => Some("__global__".into()),
        _ => None,
    }
}

async fn call_devtools_tool(
    bridge: &Arc<Mutex<DevtoolsBridge>>,
    tool_name: &str,
    arguments: Value,
    timeout: Duration,
) -> Result<Value, String> {
    if DevtoolsBridge::has_active_user_calls(bridge).await {
        return Err("WATCHDOG_DEFERRED: user browser work is active".into());
    }
    let mutation_lock = match watchdog_mutation_key(tool_name, &arguments) {
        Some(key) => Some(DevtoolsBridge::mutation_lock(bridge, &key).await),
        None => None,
    };
    let _mutation_guard = match mutation_lock.as_ref() {
        Some(lock) => Some(lock.lock().await),
        None => None,
    };
    if DevtoolsBridge::has_active_user_calls(bridge).await {
        return Err("WATCHDOG_DEFERRED: user browser work became active".into());
    }
    let request = json!({
        "jsonrpc": "2.0",
        "id": next_watchdog_id(),
        "method": "tools/call",
        "params": {
            "name": tool_name,
            "arguments": arguments
        }
    });
    DevtoolsBridge::request_shared_with_timeout(bridge, &request, timeout).await
}

fn response_reconnected(response: &Value) -> bool {
    response
        .pointer("/result/structuredContent/reconnected")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn response_pages(response: &Value) -> Vec<(u64, String)> {
    response
        .pointer("/result/structuredContent/pages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|page| {
            let id = page.get("id")?.as_u64()?;
            let url = page.get("url")?.as_str()?.to_string();
            Some((id, url))
        })
        .collect()
}

fn response_message(response: &Value) -> Option<&str> {
    response
        .pointer("/result/structuredContent/message")
        .and_then(Value::as_str)
}

fn is_chatgpt_url(url: &str) -> bool {
    url.starts_with("https://chatgpt.com/") || url.starts_with("https://www.chatgpt.com/")
}

fn is_persistable_chatgpt_url(url: &str) -> bool {
    is_chatgpt_url(url)
        && !url.contains("/auth")
        && !url.contains("/login")
        && !url.contains("/signup")
        && url != CHATGPT_HOME_URL
        && url != "https://www.chatgpt.com/"
}

fn recovery_state_path() -> Option<PathBuf> {
    user_home_dir()
        .ok()
        .map(|home| home.join(".catdesk").join("browser-recovery.json"))
}

fn load_recovery_state() -> BrowserRecoveryState {
    let Some(path) = recovery_state_path() else {
        return BrowserRecoveryState::default();
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return BrowserRecoveryState::default();
    };
    serde_json::from_str(&text).unwrap_or_default()
}

fn save_recovery_state(state: &BrowserRecoveryState) {
    let Some(path) = recovery_state_path() else {
        return;
    };
    let Some(parent) = path.parent() else {
        return;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    let Ok(data) = serde_json::to_vec_pretty(state) else {
        return;
    };
    let _ = std::fs::write(path, data);
}

fn health_check_function() -> &'static str {
    r#"() => {
  const visible = (el) => {
    if (!(el instanceof HTMLElement)) return false;
    const s = getComputedStyle(el);
    const r = el.getBoundingClientRect();
    return s.visibility !== 'hidden' && s.display !== 'none' && r.width > 0 && r.height > 0 &&
      r.bottom > 0 && r.right > 0 && r.top < window.innerHeight && r.left < window.innerWidth;
  };
  const normalize = (value) => (value || '').replace(/\s+/g, ' ').trim().toLowerCase();
  const failureTexts = [
    'connection interrupted',
    'network error',
    'something went wrong',
    'соединение прервано',
    'ошибка сети',
    'что-то пошло не так'
  ];
  const rateLimitTexts = [
    'too many requests',
    'rate limit',
    'слишком много запросов'
  ];
  const statusNodes = Array.from(document.querySelectorAll(
    '[role="alert"],[aria-live="assertive"],[data-testid*="error"],[class*="error"]'
  )).filter(visible);
  const statusTexts = statusNodes.map((el) => normalize(el.innerText || el.textContent));
  const turnNodes = Array.from(document.querySelectorAll('[data-message-author-role]'));
  const lastTurnRect = turnNodes.length > 0
    ? turnNodes[turnNodes.length - 1].getBoundingClientRect()
    : null;
  const retryButtonNodes = Array.from(document.querySelectorAll('button')).filter((el) => {
    if (!visible(el)) return false;
    const text = normalize(el.innerText || el.textContent);
    if (text !== 'retry' && text !== 'повторить') return false;
    if (!lastTurnRect) return true;
    return el.getBoundingClientRect().top >= lastTurnRect.top - 64;
  });
  const retryContextTexts = retryButtonNodes.map((button) => {
    let node = button.parentElement;
    for (let i = 0; i < 4 && node; i += 1, node = node.parentElement) {
      const text = normalize(node.innerText || node.textContent);
      if (text.length > 0 && text.length < 1200) return text;
    }
    return '';
  });
  const rateLimited = [...statusTexts, ...retryContextTexts].some((text) =>
    rateLimitTexts.some((needle) => text.includes(needle))
  );
  const failed = !rateLimited && (retryButtonNodes.length > 0 || statusTexts.some((text) =>
    failureTexts.some((needle) => text.includes(needle))
  ));
  const userTurns = document.querySelectorAll('[data-message-author-role="user"]').length;
  const assistantNodes = Array.from(document.querySelectorAll('[data-message-author-role="assistant"]'));
  const assistantTurns = assistantNodes.length;
  const lastAssistantChars = assistantNodes.length > 0
    ? normalize(assistantNodes[assistantNodes.length - 1].innerText || assistantNodes[assistantNodes.length - 1].textContent).length
    : 0;
  return { href: location.href, failed, rateLimited, userTurns, assistantTurns, lastAssistantChars };
}"#
}

fn prepare_resume_function() -> &'static str {
    r#"() => {
  const visible = (el) => {
    if (!(el instanceof HTMLElement)) return false;
    const s = getComputedStyle(el);
    const r = el.getBoundingClientRect();
    return s.visibility !== 'hidden' && s.display !== 'none' && r.width > 0 && r.height > 0 &&
      r.bottom > 0 && r.right > 0 && r.top < window.innerHeight && r.left < window.innerWidth;
  };
  const normalize = (value) => (value || '').replace(/\s+/g, ' ').trim();
  const userTurns = document.querySelectorAll('[data-message-author-role="user"]').length;
  const assistantNodes = Array.from(document.querySelectorAll('[data-message-author-role="assistant"]'));
  const assistantTurns = assistantNodes.length;
  const lastAssistantChars = assistantNodes.length > 0
    ? normalize(assistantNodes[assistantNodes.length - 1].innerText || assistantNodes[assistantNodes.length - 1].textContent).length
    : 0;
  const busy = Array.from(document.querySelectorAll('button')).filter(visible).some((button) => {
    const marker = [
      button.getAttribute('data-testid') || '',
      button.getAttribute('aria-label') || '',
      button.innerText || button.textContent || ''
    ].join(' ').toLowerCase();
    return marker.includes('stop-button') || marker.includes('stop generating') ||
      marker.includes('stop response') || marker.includes('остановить');
  });
  if (busy) return { resumeReady: false, reason: 'busy', userTurns, assistantTurns, lastAssistantChars };

  const composer = document.querySelector(
    '#prompt-textarea, textarea[data-testid*="composer"], [contenteditable="true"][data-testid*="composer"], [contenteditable="true"][role="textbox"]'
  );
  if (!(composer instanceof HTMLElement)) {
    return { resumeReady: false, reason: 'composer-missing', userTurns, assistantTurns, lastAssistantChars };
  }
  const draft = composer instanceof HTMLTextAreaElement || composer instanceof HTMLInputElement
    ? composer.value
    : composer.innerText || composer.textContent || '';
  if (normalize(draft).length > 0) {
    return { resumeReady: false, reason: 'draft-present', userTurns, assistantTurns, lastAssistantChars };
  }
  composer.focus();
  return {
    resumeReady: document.activeElement === composer,
    reason: document.activeElement === composer ? 'ready' : 'focus-failed',
    userTurns,
    assistantTurns,
    lastAssistantChars
  };
}"#
}

fn response_script_value(response: &Value) -> Option<Value> {
    let message = response_message(response)?;
    let json_start = message.find('{')?;
    serde_json::from_str(message[json_start..].trim()).ok()
}

fn response_script_bool(response: &Value, key: &str) -> bool {
    response_script_value(response)
        .and_then(|value| value.get(key).and_then(Value::as_bool))
        .unwrap_or(false)
}

fn response_script_u64(response: &Value, key: &str) -> Option<u64> {
    response_script_value(response).and_then(|value| value.get(key).and_then(Value::as_u64))
}

fn health_response_failed(response: &Value) -> bool {
    response_script_bool(response, "failed")
}

fn health_response_rate_limited(response: &Value) -> bool {
    response_script_bool(response, "rateLimited")
}

fn unix_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn pending_resume_is_fresh(pending: &PendingResume) -> bool {
    unix_now_secs().saturating_sub(pending.created_unix_secs) <= RECOVERY_RESUME_TTL.as_secs()
}

fn select_chatgpt_page(
    pages: &[(u64, String)],
    recovery_state: &BrowserRecoveryState,
) -> Option<(u64, String)> {
    recovery_state
        .pending_resume
        .as_ref()
        .and_then(|pending| {
            pages
                .iter()
                .find(|(_, url)| url == &pending.chat_url)
                .cloned()
        })
        .or_else(|| {
            recovery_state
                .last_chatgpt_url
                .as_ref()
                .and_then(|last_url| pages.iter().find(|(_, url)| url == last_url).cloned())
        })
        .or_else(|| pages.iter().find(|(_, url)| is_chatgpt_url(url)).cloned())
}

fn resume_state_has_progressed(pending: &PendingResume, response: &Value) -> bool {
    let user_turns = response_script_u64(response, "userTurns").unwrap_or(pending.user_turns);
    let assistant_turns =
        response_script_u64(response, "assistantTurns").unwrap_or(pending.assistant_turns);
    let last_assistant_chars =
        response_script_u64(response, "lastAssistantChars").unwrap_or(pending.last_assistant_chars);

    user_turns > pending.user_turns
        || assistant_turns > pending.assistant_turns
        || last_assistant_chars > pending.last_assistant_chars.saturating_add(24)
}

async fn try_resume_interrupted_chat(
    bridge: &Arc<Mutex<DevtoolsBridge>>,
    page_id: u64,
    page_url: &str,
    recovery_state: &mut BrowserRecoveryState,
    health_response: &Value,
) -> Result<bool, String> {
    let Some(pending) = recovery_state.pending_resume.clone() else {
        return Ok(false);
    };

    if pending.chat_url != page_url {
        return Ok(false);
    }

    if !pending_resume_is_fresh(&pending) || resume_state_has_progressed(&pending, health_response)
    {
        recovery_state.pending_resume = None;
        save_recovery_state(recovery_state);
        return Ok(true);
    }

    if recovery_state.last_resume_attempt_id.as_deref() == Some(pending.id.as_str()) {
        recovery_state.pending_resume = None;
        save_recovery_state(recovery_state);
        return Ok(true);
    }

    let prepare_response = call_devtools_tool(
        bridge,
        "evaluate_script",
        json!({
            "pageId": page_id,
            "function": prepare_resume_function(),
            "waitForStableDom": false
        }),
        RECOVERY_REQUEST_TIMEOUT,
    )
    .await?;

    if resume_state_has_progressed(&pending, &prepare_response) {
        recovery_state.pending_resume = None;
        save_recovery_state(recovery_state);
        return Ok(true);
    }

    if !response_script_bool(&prepare_response, "resumeReady") {
        return Ok(false);
    }

    // Mark the attempt before typing. This deliberately provides at-most-once
    // recovery: a transport timeout after Enter must never cause a duplicate
    // resume message on the next watchdog tick.
    recovery_state.last_resume_attempt_id = Some(pending.id);
    recovery_state.pending_resume = None;
    save_recovery_state(recovery_state);

    call_devtools_tool(
        bridge,
        "type_text",
        json!({
            "pageId": page_id,
            "text": RECOVERY_RESUME_MESSAGE,
            "submitKey": "Enter"
        }),
        Duration::from_secs(15),
    )
    .await?;

    Ok(true)
}

fn spawn_chatgpt_recovery_watchdog(bridge: Arc<Mutex<DevtoolsBridge>>) {
    tokio::spawn(async move {
        let mut recovery_state = load_recovery_state();
        let mut reloads = VecDeque::<Instant>::new();
        let mut cooldown_until: Option<Instant> = None;
        let mut consecutive_transport_failures = 0_u8;
        let mut consecutive_page_failures = 0_u8;
        // Always ensure a ChatGPT page exists. On the first CatDesk browser
        // launch this opens chatgpt.com; after a restart it restores the last
        // saved conversation URL when one is available.
        let mut restore_page_after_reconnect = true;

        let mut ticker = tokio::time::interval(RECOVERY_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            ticker.tick().await;

            let pages_response = match call_devtools_tool(
                &bridge,
                "list_pages",
                json!({}),
                RECOVERY_REQUEST_TIMEOUT,
            )
            .await
            {
                Ok(response) => {
                    consecutive_transport_failures = 0;
                    if response_reconnected(&response) {
                        restore_page_after_reconnect = true;
                    }
                    response
                }
                Err(error) => {
                    if error.starts_with("WATCHDOG_DEFERRED") {
                        continue;
                    }
                    consecutive_transport_failures =
                        consecutive_transport_failures.saturating_add(1);
                    if consecutive_transport_failures >= 2 {
                        if DevtoolsBridge::has_active_user_calls(&bridge).await {
                            continue;
                        }
                        let restarted = DevtoolsBridge::restart_transport_shared(&bridge)
                            .await
                            .is_ok();
                        if restarted {
                            restore_page_after_reconnect = true;
                            consecutive_transport_failures = 0;
                        }
                    }
                    continue;
                }
            };

            let pages = response_pages(&pages_response);
            let chatgpt_page = select_chatgpt_page(&pages, &recovery_state);

            let Some((page_id, page_url)) = chatgpt_page else {
                let pending_url = recovery_state
                    .pending_resume
                    .as_ref()
                    .filter(|pending| pending_resume_is_fresh(pending))
                    .map(|pending| pending.chat_url.clone());
                if restore_page_after_reconnect || pending_url.is_some() {
                    let url = pending_url
                        .or_else(|| recovery_state.last_chatgpt_url.clone())
                        .unwrap_or_else(|| CHATGPT_HOME_URL.to_string());
                    if call_devtools_tool(
                        &bridge,
                        "new_page",
                        json!({ "url": url }),
                        Duration::from_secs(20),
                    )
                    .await
                    .is_ok()
                    {
                        restore_page_after_reconnect = false;
                    }
                }
                continue;
            };

            restore_page_after_reconnect = false;
            if is_persistable_chatgpt_url(&page_url)
                && recovery_state.last_chatgpt_url.as_deref() != Some(page_url.as_str())
            {
                recovery_state.last_chatgpt_url = Some(page_url.clone());
                save_recovery_state(&recovery_state);
            }

            let health_response = match call_devtools_tool(
                &bridge,
                "evaluate_script",
                json!({
                    "pageId": page_id,
                    "function": health_check_function(),
                    "waitForStableDom": false
                }),
                RECOVERY_REQUEST_TIMEOUT,
            )
            .await
            {
                Ok(response) => response,
                Err(_) => continue,
            };

            if health_response_rate_limited(&health_response) {
                consecutive_page_failures = 0;
                cooldown_until = Some(Instant::now() + RECOVERY_COOLDOWN);
                continue;
            }

            if matches!(
                try_resume_interrupted_chat(
                    &bridge,
                    page_id,
                    &page_url,
                    &mut recovery_state,
                    &health_response,
                )
                .await,
                Ok(true)
            ) {
                consecutive_page_failures = 0;
                continue;
            }

            if recovery_state
                .pending_resume
                .as_ref()
                .is_some_and(|pending| {
                    pending.chat_url == page_url && pending_resume_is_fresh(pending)
                })
            {
                consecutive_page_failures = 0;
                continue;
            }

            if !health_response_failed(&health_response) {
                consecutive_page_failures = 0;
                continue;
            }

            consecutive_page_failures = consecutive_page_failures.saturating_add(1);
            if consecutive_page_failures < RECOVERY_REQUIRED_CONSECUTIVE_FAILURES {
                continue;
            }
            consecutive_page_failures = 0;

            let now = Instant::now();
            if cooldown_until.is_some_and(|deadline| deadline > now) {
                continue;
            }
            cooldown_until = None;

            while reloads
                .front()
                .is_some_and(|started| now.duration_since(*started) > RECOVERY_RELOAD_WINDOW)
            {
                reloads.pop_front();
            }

            if reloads.len() >= RECOVERY_MAX_RELOADS_PER_WINDOW {
                cooldown_until = Some(now + RECOVERY_COOLDOWN);
                continue;
            }

            let mut newly_armed_resume_id = None;
            if is_persistable_chatgpt_url(&page_url) {
                let has_fresh_pending =
                    recovery_state
                        .pending_resume
                        .as_ref()
                        .is_some_and(|pending| {
                            pending.chat_url == page_url && pending_resume_is_fresh(pending)
                        });
                if !has_fresh_pending {
                    let pending = PendingResume {
                        id: Uuid::new_v4().to_string(),
                        chat_url: page_url.clone(),
                        user_turns: response_script_u64(&health_response, "userTurns").unwrap_or(0),
                        assistant_turns: response_script_u64(&health_response, "assistantTurns")
                            .unwrap_or(0),
                        last_assistant_chars: response_script_u64(
                            &health_response,
                            "lastAssistantChars",
                        )
                        .unwrap_or(0),
                        created_unix_secs: unix_now_secs(),
                    };
                    newly_armed_resume_id = Some(pending.id.clone());
                    recovery_state.pending_resume = Some(pending);
                    save_recovery_state(&recovery_state);
                }
            }

            let reload_result = call_devtools_tool(
                &bridge,
                "navigate_page",
                json!({
                    "pageId": page_id,
                    "type": "reload",
                    "ignoreCache": false,
                    "timeout": 20_000
                }),
                Duration::from_secs(25),
            )
            .await;

            if reload_result.is_ok() {
                reloads.push_back(now);
            } else if let Some(new_resume_id) = newly_armed_resume_id
                && recovery_state
                    .pending_resume
                    .as_ref()
                    .is_some_and(|pending| pending.id == new_resume_id)
            {
                recovery_state.pending_resume = None;
                save_recovery_state(&recovery_state);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chatgpt_url_detection_ignores_other_sites() {
        assert!(is_chatgpt_url("https://chatgpt.com/c/abc"));
        assert!(is_chatgpt_url("https://www.chatgpt.com/"));
        assert!(!is_chatgpt_url("https://example.com/chatgpt.com/"));
    }

    #[test]
    fn persistable_chat_url_skips_auth_and_home() {
        assert!(is_persistable_chatgpt_url("https://chatgpt.com/c/abc"));
        assert!(!is_persistable_chatgpt_url("https://chatgpt.com/"));
        assert!(!is_persistable_chatgpt_url(
            "https://chatgpt.com/auth/login"
        ));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_chrome_devtools_uses_auto_connect_to_existing_profile() {
        let browser = DetectedBrowser {
            name: "Google Chrome".into(),
            binary: "google-chrome-stable".into(),
            path: r"C:\Program Files\Google\Chrome\Application\chrome.exe".into(),
            remote_debugging: true,
            remote_debug_hint: "chrome://inspect/#remote-debugging".into(),
            mcp_supported: true,
            support_note: String::new(),
            remote_debug_active: false,
            remote_debug_target: None,
            remote_debug_pid: None,
        };

        let args = build_launch_args(Some(&browser)).expect("build Windows launch args");
        assert!(args.iter().any(|arg| arg == "--autoConnect"));
        assert!(!args.iter().any(|arg| arg == "--browserUrl"));
        assert!(!args.iter().any(|arg| arg == "--executablePath"));
        assert!(!args.iter().any(|arg| arg == "--userDataDir"));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_chrome_auto_connect_ignores_stale_pipe_state() {
        let browser = DetectedBrowser {
            name: "Google Chrome".into(),
            binary: "google-chrome-stable".into(),
            path: r"C:\Program Files\Google\Chrome\Application\chrome.exe".into(),
            remote_debugging: true,
            remote_debug_hint: "chrome://inspect/#remote-debugging".into(),
            mcp_supported: true,
            support_note: String::new(),
            remote_debug_active: true,
            remote_debug_target: Some("pipe".into()),
            remote_debug_pid: Some(123),
        };

        let args = build_launch_args(Some(&browser)).expect("build Windows launch args");
        assert!(args.iter().any(|arg| arg == "--autoConnect"));
        assert!(!args.iter().any(|arg| arg == "--browserUrl"));
    }

    #[test]
    fn failed_health_response_is_detected_from_structured_message() {
        let failed = json!({
            "result": {
                "structuredContent": {
                    "message": "Script ran on page and returned:\n{\"href\":\"https://chatgpt.com/c/x\",\"failed\":true,\"rateLimited\":false}"
                }
            }
        });
        let healthy = json!({
            "result": {
                "structuredContent": {
                    "message": "Script ran on page and returned:\n{\"href\":\"https://chatgpt.com/c/x\",\"failed\":false,\"rateLimited\":false}"
                }
            }
        });
        let rate_limited = json!({
            "result": {
                "structuredContent": {
                    "message": "Script ran on page and returned:\n{\"href\":\"https://chatgpt.com/c/x\",\"failed\":false,\"rateLimited\":true}"
                }
            }
        });

        assert!(health_response_failed(&failed));
        assert!(!health_response_failed(&healthy));
        assert!(!health_response_rate_limited(&failed));
        assert!(health_response_rate_limited(&rate_limited));
    }

    fn health_response(user_turns: u64, assistant_turns: u64, last_assistant_chars: u64) -> Value {
        json!({
            "result": {
                "structuredContent": {
                    "message": format!(
                        "Script ran on page and returned:\n{{\"failed\":false,\"rateLimited\":false,\"userTurns\":{user_turns},\"assistantTurns\":{assistant_turns},\"lastAssistantChars\":{last_assistant_chars}}}"
                    )
                }
            }
        })
    }

    fn pending_resume(chat_url: &str) -> PendingResume {
        PendingResume {
            id: "resume-1".into(),
            chat_url: chat_url.into(),
            user_turns: 3,
            assistant_turns: 3,
            last_assistant_chars: 120,
            created_unix_secs: unix_now_secs(),
        }
    }

    #[test]
    fn legacy_recovery_state_without_resume_fields_still_loads() {
        let state: BrowserRecoveryState =
            serde_json::from_str(r#"{"last_chatgpt_url":"https://chatgpt.com/c/legacy"}"#)
                .expect("deserialize legacy recovery state");

        assert_eq!(
            state.last_chatgpt_url.as_deref(),
            Some("https://chatgpt.com/c/legacy")
        );
        assert!(state.pending_resume.is_none());
        assert!(state.last_resume_attempt_id.is_none());
    }

    #[test]
    fn pending_resume_is_cancelled_when_conversation_progressed() {
        let pending = pending_resume("https://chatgpt.com/c/x");

        assert!(!resume_state_has_progressed(
            &pending,
            &health_response(3, 3, 120)
        ));
        assert!(resume_state_has_progressed(
            &pending,
            &health_response(4, 3, 120)
        ));
        assert!(resume_state_has_progressed(
            &pending,
            &health_response(3, 4, 120)
        ));
        assert!(resume_state_has_progressed(
            &pending,
            &health_response(3, 3, 145)
        ));
    }

    #[test]
    fn pending_resume_allows_small_render_only_text_drift() {
        let pending = pending_resume("https://chatgpt.com/c/x");

        assert!(!resume_state_has_progressed(
            &pending,
            &health_response(3, 3, 144)
        ));
    }

    #[test]
    fn chat_selection_prefers_pending_resume_chat() {
        let pages = vec![
            (1, "https://chatgpt.com/c/other".to_string()),
            (2, "https://chatgpt.com/c/resume".to_string()),
        ];
        let state = BrowserRecoveryState {
            last_chatgpt_url: Some("https://chatgpt.com/c/other".into()),
            pending_resume: Some(pending_resume("https://chatgpt.com/c/resume")),
            last_resume_attempt_id: None,
        };

        assert_eq!(
            select_chatgpt_page(&pages, &state),
            Some((2, "https://chatgpt.com/c/resume".to_string()))
        );
    }

    #[tokio::test]
    async fn stale_generation_response_cannot_complete_new_request() {
        let pending: PendingRequests = Arc::new(Mutex::new(HashMap::new()));
        let id = json!("same-id");
        let (old_tx, old_rx) = tokio::sync::oneshot::channel();
        let (new_tx, mut new_rx) = tokio::sync::oneshot::channel();
        {
            let mut map = pending.lock().await;
            map.insert((1, id.clone()), old_tx);
            map.insert((2, id.clone()), new_tx);
        }

        route_response(&pending, 1, json!({"id":id,"result":{"generation":1}})).await;
        assert_eq!(old_rx.await.unwrap()["result"]["generation"], json!(1));
        assert!(
            tokio::time::timeout(Duration::from_millis(10), &mut new_rx)
                .await
                .is_err(),
            "old transport response must not satisfy the new generation"
        );

        route_response(
            &pending,
            2,
            json!({"id":"same-id","result":{"generation":2}}),
        )
        .await;
        assert_eq!(new_rx.await.unwrap()["result"]["generation"], json!(2));
    }

    #[tokio::test]
    async fn eof_only_fails_pending_requests_from_its_generation() {
        let pending: PendingRequests = Arc::new(Mutex::new(HashMap::new()));
        let (old_tx, old_rx) = tokio::sync::oneshot::channel();
        let (new_tx, mut new_rx) = tokio::sync::oneshot::channel();
        {
            let mut map = pending.lock().await;
            map.insert((7, json!("old")), old_tx);
            map.insert((8, json!("new")), new_tx);
        }
        fail_generation(&pending, 7).await;
        assert!(old_rx.await.is_err());
        assert!(
            tokio::time::timeout(Duration::from_millis(10), &mut new_rx)
                .await
                .is_err()
        );
        route_response(&pending, 8, json!({"id":"new","result":true})).await;
        assert_eq!(new_rx.await.unwrap()["result"], json!(true));
    }

    #[tokio::test]
    async fn page_scheduler_serializes_one_page_but_not_independent_pages() {
        let scheduler = PageMutationScheduler::default();
        let page_a_first = scheduler.lock_for("page:11").await;
        let page_a_second = scheduler.lock_for("page:11").await;
        let page_b = scheduler.lock_for("page:12").await;
        assert!(Arc::ptr_eq(&page_a_first, &page_a_second));
        assert!(!Arc::ptr_eq(&page_a_first, &page_b));

        let first_guard = page_a_first.lock().await;
        assert!(
            tokio::time::timeout(Duration::from_millis(10), page_a_second.lock())
                .await
                .is_err(),
            "same page mutations must remain sequential"
        );
        let independent_guard = tokio::time::timeout(Duration::from_millis(50), page_b.lock())
            .await
            .expect("independent page must not wait for page 11");
        drop(independent_guard);
        drop(first_guard);
        let _final_guard = tokio::time::timeout(Duration::from_millis(50), page_a_second.lock())
            .await
            .expect("same page proceeds after prior mutation finishes");
    }

    #[test]
    fn watchdog_defers_while_user_browser_work_is_active() {
        let activity = std::sync::atomic::AtomicUsize::new(0);
        assert!(!watchdog_should_defer(&activity));
        activity.store(1, Ordering::Release);
        assert!(watchdog_should_defer(&activity));
    }

    #[test]
    fn watchdog_mutation_keys_share_page_scheduler_namespace() {
        assert_eq!(
            watchdog_mutation_key("navigate_page", &json!({"pageId":42})).as_deref(),
            Some("page:42")
        );
        assert_eq!(
            watchdog_mutation_key("type_text", &json!({"pageId":42})).as_deref(),
            Some("page:42")
        );
        assert_eq!(
            watchdog_mutation_key("new_page", &json!({"url":"https://chatgpt.com"})).as_deref(),
            Some("__global__")
        );
        assert!(watchdog_mutation_key("list_pages", &json!({})).is_none());
    }

    #[test]
    fn devtools_mcp_version_is_pinned() {
        let args = build_launch_args(None).expect("launch args");
        assert!(args.iter().any(|arg| arg == "chrome-devtools-mcp@1.10.1"));
        assert!(!args.iter().any(|arg| arg.contains("@latest")));
        assert!(args.iter().any(|arg| arg == "--no-usage-statistics"));
    }

    #[test]
    fn interactive_browser_tools_have_bounded_waits() {
        assert_eq!(
            DevtoolsBridge::user_tool_timeout("evaluate_script"),
            Duration::from_secs(15)
        );
        assert_eq!(
            DevtoolsBridge::user_tool_timeout("take_screenshot"),
            Duration::from_secs(20)
        );
        assert_eq!(
            DevtoolsBridge::user_tool_timeout("navigate_page"),
            Duration::from_secs(35)
        );
        assert_eq!(
            DevtoolsBridge::user_tool_timeout("performance_start_trace"),
            DEVTOOLS_REQUEST_TIMEOUT
        );
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn waiting_for_response_does_not_hold_bridge_mutex() {
        use std::process::Stdio;

        let mut child = Command::new("cmd.exe")
            .args(["/D", "/Q", "/C", "more"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn inert child");
        let stdin = tokio::io::BufWriter::new(child.stdin.take().expect("stdin"));
        let pending: PendingRequests = Arc::new(Mutex::new(HashMap::new()));
        let bridge = Arc::new(Mutex::new(DevtoolsBridge {
            child,
            stdin,
            pending,
            launch_args: Vec::new(),
            generation: 1,
            tools_cache: None,
            active_user_calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            page_scheduler: PageMutationScheduler::default(),
            tools_refresh: Arc::new(Mutex::new(())),
        }));

        let waiting = {
            let bridge = bridge.clone();
            tokio::spawn(async move {
                DevtoolsBridge::request_shared_with_timeout(
                    &bridge,
                    &json!({"jsonrpc":"2.0","id":"never-answered","method":"noop"}),
                    Duration::from_millis(150),
                )
                .await
            })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;

        let short_lock = tokio::time::timeout(Duration::from_millis(30), bridge.lock())
            .await
            .expect("response wait must not retain the bridge write mutex");
        drop(short_lock);

        let error = waiting
            .await
            .expect("wait task")
            .expect_err("inert child never returns JSON-RPC");
        assert!(error.contains("timed out"));
        DevtoolsBridge::stop_shared(&bridge).await;
    }
}
