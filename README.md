# claude-monitor

See, at a glance, every Claude Code instance running across your Zellij sessions
— whether each is **idle**, **working**, or **waiting for input** — and jump
straight to the pane one is running in.

Two pieces plus a hook config:

- **`server/`** — a tiny local HTTP server (`axum`) that holds the live status
  of each instance in memory.
- **`plugin/`** — a Zellij plugin (WASM) that renders the instances as a
  selectable list. Selecting one switches to its session and focuses its pane.
- **`hooks.json`** — Claude Code native HTTP hooks that report status to the
  server. No shell script, no extra binary.

## How it works

```
Claude Code (in a zellij pane)
  └─ hooks (http + command)  ──POST event JSON + X-Zellij-* headers──►  ┌──────────────┐
     SessionStart/UserPromptSubmit/PreToolUse/PermissionRequest/Stop/End │ server 47100 │
                                                                         └──────────────┘
zellij plugin (one per session)  ◄────────── GET /state ──────────────────────┘
  • polls /state every second       • SessionUpdate → live pane list (all sessions)
  • drops instances whose pane is gone   • Enter → switch_session_with_focus(...)
```

- The server is **client-agnostic**: it keys instances by their Claude Code
  `session_id` (present in every hook body) and doesn't care whether an instance
  runs under Zellij. Zellij *location* metadata — the session name and pane id —
  rides along in the `X-Zellij-Session` / `X-Zellij-Pane` headers (interpolated
  from `$ZELLIJ_SESSION_NAME` / `$ZELLIJ_PANE_ID`) and is stored as optional
  fields. The **plugin** filters: it only shows instances that carry Zellij
  metadata *and* match a live pane, so the same server could back other clients.
- All events are `http` hooks **except `SessionStart`, which is a `command`
  (curl) hook.** A `SessionStart` HTTP hook fires so early in startup that it gets
  dropped and the report never arrives; running it as a synchronous command hook
  fixes that, so a new instance shows up as *idle* before its first prompt. The
  rest are reliable as HTTP hooks.
- Waiting uses **`PermissionRequest`** (fires the instant a permission dialog
  appears) and **`Elicitation`** (an MCP tool asking for input) — *not*
  `Notification`, which is informational and fires several seconds late by design.
  That wrong-event choice, not the HTTP transport, was the source of the earlier
  "delayed sound".
- The server maps each `hook_event_name` to a status:
  `SessionStart`/`Stop` → idle, `UserPromptSubmit`/`PreToolUse` → working,
  `PermissionRequest`/`Elicitation` → waiting, `SessionEnd` → removed.
- `PreToolUse` also reads `tool_name`: tools that immediately block on the user —
  `AskUserQuestion` and `ExitPlanMode` — map to **waiting** instead of working,
  so the monitor shows (and dings on) those too.
- Liveness is self-healing but best-effort: the plugin cross-checks each
  reported instance against Zellij's own `SessionUpdate` pane list and drops any
  whose pane is positively gone — so crashed/killed instances disappear even if
  no hook fired. Instances in sessions Zellij hasn't reported to the plugin yet
  (cross-session info can lag right after the plugin opens) are shown anyway,
  trusting the server, rather than hidden until the data arrives.

## Build

```sh
# server (host target)
cargo build --release -p claude-monitor-server

# plugin (wasm) — needs the wasm32-wasip1 std once:
#   rustup (Linux/macOS): rustup target add wasm32-wasip1
#   arch:                 pacman -S rust-wasm   (or use rustup)
cargo build --release -p claude-monitor-plugin --target wasm32-wasip1
```

Artifacts:
- `target/release/claude-monitor-server`
- `target/wasm32-wasip1/release/claude-monitor-plugin.wasm`

## Install & run

**1. Run the server** (any time before/after starting sessions):

```sh
./target/release/claude-monitor-server            # binds 127.0.0.1:47100
# override the port with --port 40000 or CLAUDE_MONITOR_PORT=40000
```

Run it in the background however you like (a `zellij run`, a `&`, a systemd user
unit on Linux, or a launchd LaunchAgent on macOS — see the macOS notes below). It
keeps no persistent state, so restarting it just clears the list until instances
report again.

**2. Add the hooks** — merge `hooks.json` into `~/.claude/settings.json` (under
the top-level `hooks` key). If Claude isn't running inside Zellij the
`X-Zellij-Session` header is empty and the server ignores the report, so this is
safe to enable globally.

**3. Put the plugin in a layout** as a tiled side pane. In your layout `.kdl`
(e.g. `~/.config/zellij/layouts/default.kdl`), give it a width:

```kdl
layout {
    pane size=34 {
        plugin location="file:/absolute/path/to/claude-monitor-plugin.wasm"
        // server_url "http://127.0.0.1:47100"   // optional override
    }
    pane   // your work panes go here
}
```

Or bind a key to open it on demand in `~/.config/zellij/config.kdl`:

```kdl
keybinds {
    shared_except "locked" {
        bind "Ctrl y" {
            LaunchOrFocusPlugin "file:/absolute/path/to/claude-monitor-plugin.wasm" {
                floating true
                // server_url "http://127.0.0.1:47100"   // optional override
            }
        }
    }
}
```

On first launch Zellij prompts to grant the plugin permissions
(ReadApplicationState, ChangeApplicationState, WebAccess) — accept them.

## Server API

`POST /report` — Claude Code hooks post their event JSON here (identity comes
from the `X-Zellij-*` headers). `GET /state` — returns `{ "instances": [...] }`.
Any client can poll it. Each instance:

| Field | Notes |
|-------|-------|
| `session_id` | Claude Code session id — the instance key |
| `status` | `idle` \| `working` \| `waiting` |
| `cwd` | working directory |
| `model` | model in use (from `SessionStart`) |
| `title` | session title, if set (from `SessionStart`) |
| `permission_mode` | `default` / `plan` / `acceptEdits` / `bypassPermissions` / … |
| `transcript_path` | path to the conversation JSONL |
| `agent_type` | custom agent name, if launched with `--agent` |
| `zellij_session`, `zellij_pane` | Zellij location — omitted for non-Zellij instances |

Empty fields are omitted. The Zellij plugin only uses `status`, `cwd`,
`zellij_session`, `zellij_pane`; the rest are carried for other clients.

## Use

With the plugin focused:

- `↑`/`↓` (or `k`/`j`) — move the selection
- `⏎` — jump to the selected instance (switches session if needed, otherwise
  just focuses the pane)
- `q` / `Esc` — close the plugin

## Sound on halt

The server can play a sound when an instance transitions **from working to a
halted state** — i.e. `working → idle` (finished responding) or
`working → waiting` (needs your input). Set `CLAUDE_MONITOR_SOUND` to a shell
command when launching the server; leave it unset to disable (the default).

```sh
# Linux (PipeWire/PulseAudio)
CLAUDE_MONITOR_SOUND='paplay /usr/share/sounds/freedesktop/stereo/complete.oga' \
  ./target/release/claude-monitor-server

# macOS
CLAUDE_MONITOR_SOUND='afplay /System/Library/Sounds/Glass.aiff' \
  ./target/release/claude-monitor-server
```

The command runs via `sh -c` and only fires on the working→halt edge (not on
every event, and not when an instance starts idle). These env vars are available
to it, so a script can vary the sound:

- `CLAUDE_MONITOR_STATUS` — `idle` or `waiting`
- `CLAUDE_MONITOR_SESSION_ID` — the Claude Code session id
- `CLAUDE_MONITOR_ZELLIJ_SESSION` — the Zellij session name (empty if not in Zellij)
- `CLAUDE_MONITOR_ZELLIJ_PANE` — the Zellij pane id (empty if not in Zellij)

To enable it under a service manager, add `CLAUDE_MONITOR_SOUND` to the unit's
environment (the systemd user unit's `Environment=` or the launchd plist's
`EnvironmentVariables` dict). Note a background service may need access to your
audio session for the player to work.

**Latency:** the player itself starts in ~100 ms, so if the ding feels late the
usual cause is either (a) a long/soft sound — use one with an immediate attack,
e.g. `canberra-gtk-play -i message` instead of the ~1.1 s `complete.oga` — or
(b) the wrong *event*. The waiting ding uses `PermissionRequest`, which fires the
instant the permission dialog appears, rather than `Notification`, which is
deliberately delayed.

## Config

| Where            | Setting                | Default                   |
|------------------|------------------------|---------------------------|
| server           | `--port` / `CLAUDE_MONITOR_PORT` | `47100`         |
| server           | `CLAUDE_MONITOR_SOUND` (sound on halt) | unset (off) |
| server           | `--log` / `CLAUDE_MONITOR_LOG` (log each report to stdout) | off |
| plugin           | `server_url` config    | `http://127.0.0.1:47100`  |
| hooks (`.json`)  | `url`                  | `http://127.0.0.1:47100/report` |

If you change the port, update all three.

## macOS notes

Everything works the same on macOS — Zellij sets `ZELLIJ_SESSION_NAME` /
`ZELLIJ_PANE_ID` in every pane, the config lives at `~/.config/zellij/config.kdl`,
and `curl` ships with the OS. Two differences:

**Toolchain (Homebrew + rustup).** There's no distro `rust-wasm` package; use
rustup for the wasm target:

```sh
brew install zellij
brew install rustup-init && rustup-init -y   # or: brew install rust + rustup
rustup target add wasm32-wasip1
```

**Keep the server running with launchd** (there's no systemd). Create
`~/Library/LaunchAgents/com.claude-monitor.server.plist` — use the *absolute*
path to your built binary:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>com.claude-monitor.server</string>
  <key>ProgramArguments</key>
  <array>
    <string>/Users/you/repos/claude-monitor/target/release/claude-monitor-server</string>
  </array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardOutPath</key><string>/tmp/claude-monitor.log</string>
  <key>StandardErrorPath</key><string>/tmp/claude-monitor.log</string>
  <!-- optional: override the port
  <key>EnvironmentVariables</key>
  <dict><key>CLAUDE_MONITOR_PORT</key><string>47100</string></dict>
  -->
</dict>
</plist>
```

Load / unload it (works on modern macOS):

```sh
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/com.claude-monitor.server.plist
launchctl bootout   gui/$(id -u)/com.claude-monitor.server        # to stop/remove
```

The server binds `127.0.0.1` (loopback only), so the macOS application firewall
won't prompt to allow incoming connections.
