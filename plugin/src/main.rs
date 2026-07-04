//! claude-monitor-plugin
//!
//! A Zellij plugin that lists Claude Code instances running in any Zellij
//! session along with their status (idle / working / waiting), and lets the
//! user jump to the pane an instance is running in.
//!
//! Status comes from the local claude-monitor-server (polled over HTTP via
//! `web_request`). Liveness and the tab position needed to focus a pane come
//! from Zellij's own `SessionUpdate` event, which reports panes across *all*
//! live sessions. Any reported instance whose pane no longer exists is dropped.

use std::collections::BTreeMap;

use serde::Deserialize;
use zellij_tile::prelude::*;

const DEFAULT_SERVER_URL: &str = "http://127.0.0.1:47100";
const POLL_INTERVAL_SECS: f64 = 1.0;

#[derive(Deserialize, Clone)]
struct ReportedInstance {
    session: String,
    pane_id: u32,
    status: String,
    #[serde(default)]
    cwd: String,
}

#[derive(Deserialize)]
struct StateResponse {
    instances: Vec<ReportedInstance>,
}

/// A reported instance we're going to display. `tab` is the tab position of its
/// pane when known from `SessionUpdate` (used to focus it), or `None` when we
/// don't yet have session info for its session.
struct VisibleEntry {
    inst: ReportedInstance,
    tab: Option<usize>,
    is_current_session: bool,
}

/// Result of locating a reported instance's pane in the `SessionUpdate` data.
enum PaneLookup {
    /// The session is known and the pane is live.
    Alive { tab: usize, is_current: bool },
    /// The session is known but the pane is gone — the instance is dead.
    Dead,
    /// We have no `SessionUpdate` info for this session yet (e.g. Zellij hasn't
    /// populated cross-session info). Trust the server and show it anyway.
    UnknownSession,
}

#[derive(Default)]
struct State {
    server_url: String,
    reported: Vec<ReportedInstance>,
    sessions: Vec<SessionInfo>,
    selected: usize,
}

register_plugin!(State);

impl ZellijPlugin for State {
    fn load(&mut self, configuration: BTreeMap<String, String>) {
        self.server_url = configuration
            .get("server_url")
            .cloned()
            .unwrap_or_else(|| DEFAULT_SERVER_URL.to_string());

        request_permission(&[
            PermissionType::ReadApplicationState,
            PermissionType::ChangeApplicationState,
            PermissionType::WebAccess,
        ]);
        subscribe(&[
            EventType::SessionUpdate,
            EventType::WebRequestResult,
            EventType::Timer,
            EventType::Key,
            EventType::PermissionRequestResult,
        ]);

        // Kick off the first poll immediately.
        set_timeout(0.0);
    }

    fn update(&mut self, event: Event) -> bool {
        match event {
            Event::Timer(_) => {
                self.poll();
                set_timeout(POLL_INTERVAL_SECS);
                false
            }
            Event::WebRequestResult(status, _headers, body, _context) => {
                if status == 200 {
                    if let Ok(parsed) = serde_json::from_slice::<StateResponse>(&body) {
                        self.reported = parsed.instances;
                        self.clamp_selection();
                        return true;
                    }
                }
                false
            }
            Event::SessionUpdate(sessions, _resurrectable) => {
                self.sessions = sessions;
                self.clamp_selection();
                true
            }
            Event::PermissionRequestResult(_) => {
                // Permissions may have just been granted; poll now.
                self.poll();
                false
            }
            Event::Key(key) => self.handle_key(key),
            _ => false,
        }
    }

    fn render(&mut self, _rows: usize, cols: usize) {
        let visible = self.visible();
        let w = Some(cols);
        print_text_with_coordinates(Text::new("Claude instances").color_range(2, ..), 0, 0, w, None);
        if visible.is_empty() {
            print_text_with_coordinates(Text::new("(no active instances)").dim_all(), 0, 2, w, None);
            return;
        }
        for (i, entry) in visible.iter().enumerate() {
            let text = row_text(entry, i == self.selected);
            print_text_with_coordinates(text, 0, i + 2, w, None);
        }
        let hint = Text::new("↑/↓ select   ⏎ jump   q/esc close").dim_all();
        print_text_with_coordinates(hint, 0, visible.len() + 3, w, None);
    }
}

impl State {
    fn poll(&self) {
        web_request(
            format!("{}/state", self.server_url),
            HttpVerb::Get,
            BTreeMap::new(),
            Vec::new(),
            BTreeMap::new(),
        );
    }

    /// Instances to display. Liveness is best-effort: we only drop an instance
    /// when we positively know its pane is gone (its session is present in
    /// `SessionUpdate` but the pane isn't). If we have no info for its session
    /// yet — common right after opening the plugin, before Zellij has populated
    /// cross-session info — we still show it, trusting the server.
    fn visible(&self) -> Vec<VisibleEntry> {
        let mut out = Vec::new();
        for inst in &self.reported {
            let (tab, is_current_session) = match self.locate_pane(&inst.session, inst.pane_id) {
                PaneLookup::Alive { tab, is_current } => (Some(tab), is_current),
                PaneLookup::UnknownSession => (None, false),
                PaneLookup::Dead => continue,
            };
            out.push(VisibleEntry {
                inst: inst.clone(),
                tab,
                is_current_session,
            });
        }
        out
    }

    /// Locate a terminal pane by (session name, pane id) in the `SessionUpdate`
    /// data. Returns `UnknownSession` when we have no info for that session.
    fn locate_pane(&self, session: &str, pane_id: u32) -> PaneLookup {
        let Some(session) = self.sessions.iter().find(|s| s.name == session) else {
            return PaneLookup::UnknownSession;
        };
        for (tab, panes) in &session.panes.panes {
            if panes.iter().any(|p| p.id == pane_id && !p.is_plugin) {
                return PaneLookup::Alive {
                    tab: *tab,
                    is_current: session.is_current_session,
                };
            }
        }
        PaneLookup::Dead
    }

    fn clamp_selection(&mut self) {
        let len = self.visible().len();
        if len == 0 {
            self.selected = 0;
        } else if self.selected >= len {
            self.selected = len - 1;
        }
    }

    fn handle_key(&mut self, key: KeyWithModifier) -> bool {
        let len = self.visible().len();
        match key.bare_key {
            BareKey::Up | BareKey::Char('k') => {
                self.selected = self.selected.saturating_sub(1);
                true
            }
            BareKey::Down | BareKey::Char('j') => {
                if len > 0 && self.selected + 1 < len {
                    self.selected += 1;
                }
                true
            }
            BareKey::Enter => {
                self.activate();
                false
            }
            BareKey::Esc | BareKey::Char('q') => {
                close_self();
                false
            }
            _ => false,
        }
    }

    fn activate(&self) {
        let visible = self.visible();
        let Some(entry) = visible.get(self.selected) else {
            return;
        };
        if entry.is_current_session {
            // Already here: just focus the pane, no session switch.
            focus_terminal_pane(entry.inst.pane_id, false, false);
        } else {
            // `tab` may be None if we don't have session info yet; Zellij will
            // still locate the pane by id.
            switch_session_with_focus(
                &entry.inst.session,
                entry.tab,
                Some((entry.inst.pane_id, false)),
            );
        }
        close_self();
    }
}

/// Build a row as a Zellij `Text` element. Zellij renders `.selected()`
/// highlighting and `color_range` colors through its own theme, so we avoid
/// hand-rolled ANSI (reverse video swapped FG/BG and mid-string resets clobbered
/// attributes). The status dot is colored per status; the cwd is dimmed.
fn row_text(entry: &VisibleEntry, selected: bool) -> Text {
    let known = matches!(entry.inst.status.as_str(), "idle" | "working" | "waiting");
    let glyph = if known { "●" } else { "○" };
    let cwd = shorten(&entry.inst.cwd);
    let content = format!(
        "{glyph} {status:<7} {session}  {cwd}",
        status = entry.inst.status,
        session = entry.inst.session,
    );

    let mut text = Text::new(&content);
    // Color just the status dot (first char) using theme palette colors.
    text = match entry.inst.status.as_str() {
        "waiting" => text.error_color_range(0..1), // red (theme error color)
        "working" => text.color_range(2, 0..1),
        "idle" => text.color_range(1, 0..1),
        _ => text.dim_range(0..1),
    };
    // Dim the trailing cwd.
    let cwd_start = content.chars().count().saturating_sub(cwd.chars().count());
    text = text.dim_range(cwd_start..);
    if selected {
        text = text.selected();
    }
    text
}

/// Show the last two path components of a cwd to keep rows short.
fn shorten(cwd: &str) -> String {
    let parts: Vec<&str> = cwd.trim_end_matches('/').split('/').collect();
    let n = parts.len();
    if n >= 2 {
        format!("…/{}/{}", parts[n - 2], parts[n - 1])
    } else {
        cwd.to_string()
    }
}
