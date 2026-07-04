//! claude-monitor-server
//!
//! An in-memory HTTP server that tracks the status of Claude Code instances
//! running across Zellij panes. Claude Code's native HTTP hooks POST their
//! event JSON here; the Zellij plugin polls `GET /state` to render the list.
//!
//! Instance identity comes from the `X-Zellij-Session` / `X-Zellij-Pane`
//! request headers (injected by the hook via env interpolation). Status is
//! derived from the `hook_event_name` field of the event JSON body.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::Json,
    routing::{get, post},
    Router,
};
use serde::Serialize;
use serde_json::{json, Value};

type InstanceKey = (String, u32);

#[derive(Clone, Serialize)]
struct Instance {
    session: String,
    pane_id: u32,
    status: &'static str,
    cwd: String,
    session_id: String,
    updated_at: u128,
}

struct AppState {
    instances: Mutex<HashMap<InstanceKey, Instance>>,
    /// Shell command run when an instance transitions from `working` to a
    /// halted state (`idle`/`waiting`). Read once from `CLAUDE_MONITOR_SOUND`;
    /// `None` disables the sound. See `play_sound`.
    sound_cmd: Option<String>,
}

type SharedState = Arc<AppState>;

#[tokio::main]
async fn main() {
    let port = resolve_port();
    let sound_cmd = std::env::var("CLAUDE_MONITOR_SOUND")
        .ok()
        .filter(|s| !s.trim().is_empty());
    if sound_cmd.is_some() {
        println!("sound on working→halt enabled (CLAUDE_MONITOR_SOUND)");
    }
    let state: SharedState = Arc::new(AppState {
        instances: Mutex::new(HashMap::new()),
        sound_cmd,
    });

    let app = Router::new()
        .route("/report", post(report))
        .route("/state", get(state_handler))
        .with_state(state);

    let addr = format!("127.0.0.1:{port}");
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .unwrap_or_else(|e| panic!("failed to bind {addr}: {e}"));
    println!("claude-monitor-server listening on http://{addr}");
    axum::serve(listener, app).await.unwrap();
}

/// Port precedence: `--port N` arg, then `CLAUDE_MONITOR_PORT`, then 47100.
fn resolve_port() -> u16 {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--port" {
            if let Some(v) = args.next().and_then(|v| v.parse().ok()) {
                return v;
            }
        } else if let Some(v) = arg.strip_prefix("--port=") {
            if let Ok(v) = v.parse() {
                return v;
            }
        }
    }
    std::env::var("CLAUDE_MONITOR_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(47100)
}

/// Map a Claude Code hook event name to a display status, or `None` if the
/// event should remove the instance (SessionEnd) or be ignored.
enum Action {
    Set(&'static str),
    Remove,
    Ignore,
}

fn action_for(event: &str) -> Action {
    match event {
        "SessionStart" => Action::Set("idle"),
        "Stop" | "SubagentStop" => Action::Set("idle"),
        "UserPromptSubmit" | "PreToolUse" | "PostToolUse" => Action::Set("working"),
        "Notification" => Action::Set("waiting"),
        "SessionEnd" => Action::Remove,
        _ => Action::Ignore,
    }
}

async fn report(
    State(state): State<SharedState>,
    headers: HeaderMap,
    body: Bytes,
) -> StatusCode {
    let session = header_str(&headers, "x-zellij-session");
    // Not running inside Zellij (or misconfigured hook): nothing to track.
    if session.is_empty() {
        return StatusCode::OK;
    }
    let pane_id: u32 = header_str(&headers, "x-zellij-pane").parse().unwrap_or(0);

    let payload: Value = serde_json::from_slice(&body).unwrap_or_else(|_| json!({}));
    let event = payload
        .get("hook_event_name")
        .and_then(Value::as_str)
        .unwrap_or("");
    let cwd = payload
        .get("cwd")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let session_id = payload
        .get("session_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    let key: InstanceKey = (session.clone(), pane_id);
    let mut map = state.instances.lock().unwrap();
    match action_for(event) {
        Action::Set(status) => {
            let was_working = map.get(&key).map(|i| i.status) == Some("working");
            let halted = status == "idle" || status == "waiting";
            if was_working && halted {
                play_sound(&state.sound_cmd, status, &session, pane_id);
            }
            map.insert(
                key,
                Instance {
                    session,
                    pane_id,
                    status,
                    cwd,
                    session_id,
                    updated_at: now_millis(),
                },
            );
        }
        Action::Remove => {
            map.remove(&key);
        }
        Action::Ignore => {}
    }
    StatusCode::OK
}

/// Fire the configured sound command (if any) for a working→halt transition.
/// Runs `sh -c "$cmd"` detached, with the transition details exposed as env
/// vars so a script can vary the sound. Spawned on the runtime and reaped by an
/// awaiting task so it never blocks the request or leaves a zombie.
fn play_sound(cmd: &Option<String>, status: &'static str, session: &str, pane: u32) {
    let Some(cmd) = cmd.clone() else {
        return;
    };
    let session = session.to_string();
    tokio::spawn(async move {
        let _ = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(&cmd)
            .env("CLAUDE_MONITOR_STATUS", status)
            .env("CLAUDE_MONITOR_SESSION", session)
            .env("CLAUDE_MONITOR_PANE", pane.to_string())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await;
    });
}

async fn state_handler(State(state): State<SharedState>) -> Json<Value> {
    let map = state.instances.lock().unwrap();
    let mut instances: Vec<Instance> = map.values().cloned().collect();
    instances.sort_by(|a, b| a.session.cmp(&b.session).then(a.pane_id.cmp(&b.pane_id)));
    Json(json!({ "instances": instances }))
}

fn header_str(headers: &HeaderMap, name: &str) -> String {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string()
}

fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}
