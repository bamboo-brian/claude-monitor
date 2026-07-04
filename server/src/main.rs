//! claude-monitor-server
//!
//! An in-memory HTTP server that tracks the status of Claude Code instances.
//! Claude Code hooks POST their event JSON here; clients (e.g. the Zellij
//! plugin) poll `GET /state`.
//!
//! Instances are keyed by their Claude Code `session_id` — present in every hook
//! body — so the server is client-agnostic and doesn't care whether an instance
//! runs under Zellij. Client-specific *location* metadata (for Zellij: the
//! session name and pane id, from the `X-Zellij-*` headers) is stored alongside
//! for clients that can use it, but is optional. Filtering to a particular
//! client's instances is the client's job (the Zellij plugin only shows
//! instances it can match to a live pane).

use std::collections::HashMap;
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::{
    body::Bytes,
    extract::State,
    http::HeaderMap,
    response::Json,
    routing::{get, post},
    Router,
};
use serde::Serialize;
use serde_json::{json, Value};

#[derive(Clone, Serialize)]
struct Instance {
    /// Claude Code session id — the universal instance key.
    session_id: String,
    status: &'static str,
    cwd: String,
    /// Zellij location metadata (empty / `None` for non-Zellij clients).
    #[serde(skip_serializing_if = "String::is_empty")]
    zellij_session: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    zellij_pane: Option<u32>,
    updated_at: u128,
}

struct AppState {
    /// Keyed by Claude Code `session_id`.
    instances: Mutex<HashMap<String, Instance>>,
    /// Shell command run when an instance transitions from `working` to a
    /// halted state (`idle`/`waiting`). Read once from `CLAUDE_MONITOR_SOUND`;
    /// `None` disables it. See `play_sound`.
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

/// What a hook event does to the tracked instance.
enum Action {
    Set(&'static str),
    Remove,
    Ignore,
}

/// Built-in tools that block on the user the moment they're invoked, so their
/// `PreToolUse` means "waiting for input" rather than "working".
fn is_input_tool(tool_name: &str) -> bool {
    matches!(tool_name, "AskUserQuestion" | "ExitPlanMode")
}

fn action_for(event: &str, tool_name: &str) -> Action {
    match event {
        "SessionStart" => Action::Set("idle"),
        "Stop" | "SubagentStop" => Action::Set("idle"),
        "UserPromptSubmit" | "PostToolUse" => Action::Set("working"),
        // A tool that immediately blocks on the user (AskUserQuestion / plan
        // approval) means waiting; any other tool means actively working.
        "PreToolUse" if is_input_tool(tool_name) => Action::Set("waiting"),
        "PreToolUse" => Action::Set("working"),
        // Waiting for the user: the moment a permission dialog appears, or an MCP
        // tool elicits input. (Not `Notification` — informational, fires late.)
        "PermissionRequest" | "Elicitation" => Action::Set("waiting"),
        "SessionEnd" => Action::Remove,
        _ => Action::Ignore,
    }
}

async fn report(State(state): State<SharedState>, headers: HeaderMap, body: Bytes) -> Json<Value> {
    let payload: Value = serde_json::from_slice(&body).unwrap_or_else(|_| json!({}));
    let event = str_field(&payload, "hook_event_name");
    let session_id = str_field(&payload, "session_id").to_string();
    let cwd = str_field(&payload, "cwd").to_string();
    let tool_name = str_field(&payload, "tool_name");

    // Optional client (Zellij) location metadata.
    let zellij_session = header_str(&headers, "x-zellij-session");
    let zellij_pane: Option<u32> = {
        let s = header_str(&headers, "x-zellij-pane");
        (!s.is_empty()).then(|| s.parse().ok()).flatten()
    };

    // The session id is the key; without it there's nothing to track.
    if session_id.is_empty() {
        log_report(event, "-", &zellij_session, "ignored (no session_id)");
        return Json(json!({}));
    }

    let outcome = {
        let mut map = state.instances.lock().unwrap();
        match action_for(event, tool_name) {
            Action::Set(status) => {
                let was_working = map.get(&session_id).map(|i| i.status) == Some("working");
                let sounded = was_working && (status == "idle" || status == "waiting");
                if sounded {
                    play_sound(&state.sound_cmd, status, &session_id, &zellij_session, zellij_pane);
                }
                map.insert(
                    session_id.clone(),
                    Instance {
                        session_id: session_id.clone(),
                        status,
                        cwd,
                        zellij_session: zellij_session.clone(),
                        zellij_pane,
                        updated_at: now_millis(),
                    },
                );
                if sounded {
                    format!("{status} [SOUND]")
                } else {
                    status.to_string()
                }
            }
            Action::Remove => {
                map.remove(&session_id);
                "removed".to_string()
            }
            Action::Ignore => "ignored".to_string(),
        }
    };
    log_report(event, short_id(&session_id), &zellij_session, &outcome);
    // Empty-object decision so HTTP decision hooks (e.g. PermissionRequest) read
    // "no opinion" and the normal flow proceeds.
    Json(json!({}))
}

async fn state_handler(State(state): State<SharedState>) -> Json<Value> {
    let map = state.instances.lock().unwrap();
    let mut instances: Vec<Instance> = map.values().cloned().collect();
    instances.sort_by(|a, b| {
        a.zellij_session
            .cmp(&b.zellij_session)
            .then(a.zellij_pane.cmp(&b.zellij_pane))
            .then(a.session_id.cmp(&b.session_id))
    });
    Json(json!({ "instances": instances }))
}

/// Fire the configured sound command (if any) for a working→halt transition.
/// Runs `sh -c "$cmd"` detached, with the transition details exposed as env
/// vars so a script can vary the sound. Spawned on the runtime and reaped by an
/// awaiting task so it never blocks the request or leaves a zombie.
fn play_sound(
    cmd: &Option<String>,
    status: &'static str,
    session_id: &str,
    zellij_session: &str,
    zellij_pane: Option<u32>,
) {
    let Some(cmd) = cmd.clone() else {
        return;
    };
    let session_id = session_id.to_string();
    let zellij_session = zellij_session.to_string();
    let zellij_pane = zellij_pane.map(|p| p.to_string()).unwrap_or_default();
    tokio::spawn(async move {
        let _ = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(&cmd)
            .env("CLAUDE_MONITOR_STATUS", status)
            .env("CLAUDE_MONITOR_SESSION_ID", session_id)
            .env("CLAUDE_MONITOR_ZELLIJ_SESSION", zellij_session)
            .env("CLAUDE_MONITOR_ZELLIJ_PANE", zellij_pane)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await;
    });
}

/// Log an inbound report to stdout with a millisecond timestamp, flushing so it
/// appears immediately even when stdout is redirected to a file (block-buffered).
fn log_report(event: &str, id: &str, zellij_session: &str, outcome: &str) {
    let z = if zellij_session.is_empty() {
        "-"
    } else {
        zellij_session
    };
    let mut out = std::io::stdout().lock();
    let _ = writeln!(
        out,
        "[{}] report event={event:<16} id={id:<8} zellij={z} -> {outcome}",
        hms_millis(),
    );
    let _ = out.flush();
}

fn str_field<'a>(payload: &'a Value, key: &str) -> &'a str {
    payload.get(key).and_then(Value::as_str).unwrap_or("")
}

fn header_str(headers: &HeaderMap, name: &str) -> String {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string()
}

/// First 8 chars of a session id, for compact log lines.
fn short_id(session_id: &str) -> &str {
    &session_id[..session_id.len().min(8)]
}

/// Current wall-clock time of day as `HH:MM:SS.mmm` (UTC). Deltas between lines
/// are exact, which is what matters for eyeballing latency.
fn hms_millis() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    let (h, m, s) = ((secs / 3600) % 24, (secs / 60) % 60, secs % 60);
    format!("{h:02}:{m:02}:{s:02}.{:03}", now.subsec_millis())
}

fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}
