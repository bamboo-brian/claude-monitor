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
        // Waiting for the user: fired the moment a permission dialog appears, or
        // when an MCP tool elicits input. (Not `Notification` — that one is
        // informational and fires seconds late by design.)
        "PermissionRequest" | "Elicitation" => Action::Set("waiting"),
        "SessionEnd" => Action::Remove,
        _ => Action::Ignore,
    }
}

async fn report(
    State(state): State<SharedState>,
    headers: HeaderMap,
    body: Bytes,
) -> Json<Value> {
    let session = header_str(&headers, "x-zellij-session");
    let pane_str = header_str(&headers, "x-zellij-pane");

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
    // Present on tool events (PreToolUse/PostToolUse); lets us treat
    // input-blocking tools as "waiting".
    let tool_name = payload
        .get("tool_name")
        .and_then(Value::as_str)
        .unwrap_or("");

    // Not running inside Zellij (or misconfigured hook): nothing to track.
    if session.is_empty() {
        log_report(event, "-", &pane_str, "ignored (no zellij session)");
        return Json(json!({}));
    }
    let pane_id: u32 = pane_str.parse().unwrap_or(0);

    let key: InstanceKey = (session.clone(), pane_id);
    let outcome = {
        let mut map = state.instances.lock().unwrap();
        match action_for(event, tool_name) {
            Action::Set(status) => {
                let was_working = map.get(&key).map(|i| i.status) == Some("working");
                let sounded = was_working && (status == "idle" || status == "waiting");
                if sounded {
                    play_sound(&state.sound_cmd, status, &session, pane_id);
                }
                map.insert(
                    key,
                    Instance {
                        session: session.clone(),
                        pane_id,
                        status,
                        cwd,
                        session_id,
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
                map.remove(&key);
                "removed".to_string()
            }
            Action::Ignore => "ignored".to_string(),
        }
    };
    log_report(event, &session, &pane_str, &outcome);
    // Return an empty-object decision so HTTP decision hooks (e.g.
    // PermissionRequest) read "no opinion" and the normal flow proceeds.
    Json(json!({}))
}

/// Log an inbound report to stdout with a millisecond timestamp, flushing so it
/// appears immediately even when stdout is redirected to a file (block-buffered).
fn log_report(event: &str, session: &str, pane: &str, outcome: &str) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(
        out,
        "[{}] report event={event:<16} session={session} pane={pane} -> {outcome}",
        hms_millis(),
    );
    let _ = out.flush();
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
