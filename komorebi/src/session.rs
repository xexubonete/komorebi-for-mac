//! Session persistence: stores the window→workspace map so it can be
//! restored after a `rset` (restarting komorebi while the apps stay alive).
//!
//! Two things identify a window here, and they fail in different situations.
//! Accessibility ids are exact but only last as long as the process, so they
//! survive a `rset` and nothing else. App name plus title survives anything that
//! reopens the same windows -- logging out and back in, and a full reboot with
//! "reopen windows when logging back in" ticked -- at the cost of being a guess
//! when two windows of one app share a title. Both are tried, ids first.

use crate::DATA_DIR;
use crate::window_manager::WindowManager;
use serde::Deserialize;
use serde::Serialize;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::OnceLock;

#[derive(Serialize, Deserialize, Default, Debug, Clone)]
pub struct SessionState {
    /// System boot UUID (kern.bootsessionuuid). We only restore the session if
    /// it matches the current boot: after a Mac reboot window ids are reassigned
    /// and could collide by chance with old ids, placing windows on the wrong
    /// workspaces. A different boot ⇒ discard the session (windows fall back to
    /// their default workspace).
    #[serde(default)]
    pub boot_uuid: String,
    pub windows: Vec<SessionWindow>,
    /// Whether the window ids in here still refer to the windows they were
    /// written for. False once the Mac has rebooted: the placements are still
    /// worth having, but only what can be matched by name and title.
    #[serde(skip)]
    pub ids_are_current: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SessionWindow {
    pub window_id: u32,
    /// App name (komorebi exe). Lets us match by app+title when the window id is
    /// no longer valid (after logout/login the apps reopen with new ids but
    /// within the same system boot).
    #[serde(default)]
    pub exe: String,
    #[serde(default)]
    pub title: String,
    pub monitor: usize,
    pub workspace: usize,
    /// Which slot of that workspace the window occupied. Without it the workspace
    /// came back right but the grid cells were dealt out in whatever order macOS
    /// happened to enumerate the windows in -- which is by depth, so it changed
    /// with whatever had been looked at most recently.
    #[serde(default)]
    pub index: usize,
}

impl SessionState {
    /// Finds the remembered (monitor, workspace) for a window and consumes the
    /// entry (so two windows can't claim the same one). Priority:
    ///   1. window id + app  → exact, the rset case (apps still alive).
    ///   2. app + title      → best-effort, the logout/login case (new ids).
    /// Requiring the app to match avoids misplacing a window if a new id
    /// collides by chance with an old one from a different app.
    pub fn take_match(
        &mut self,
        window_id: u32,
        exe: &str,
        title: &str,
    ) -> Option<(usize, usize, usize)> {
        // Ids are only meaningful within the boot that issued them. Across a
        // reboot macOS hands the same numbers out again, so an id that matches
        // means nothing and could put a window on someone else's workspace.
        if self.ids_are_current
            && let Some(pos) = self
                .windows
                .iter()
                .position(|w| w.window_id == window_id && w.exe == exe)
        {
            let w = self.windows.remove(pos);
            return Some((w.monitor, w.workspace, w.index));
        }

        if !title.is_empty()
            && let Some(pos) = self
                .windows
                .iter()
                .position(|w| w.exe == exe && w.title == title)
        {
            let w = self.windows.remove(pos);
            return Some((w.monitor, w.workspace, w.index));
        }

        None
    }
}

fn session_path() -> PathBuf {
    DATA_DIR.join("komorebi.session.json")
}

/// Current system boot UUID, via `sysctl -n kern.bootsessionuuid`. Stable for
/// the whole boot (survives a `rset`) and different after a reboot. No extra
/// dependencies. Computed once (it doesn't change during the process lifetime)
/// because save() is called on every event.
fn boot_uuid() -> Option<String> {
    static CACHED: OnceLock<Option<String>> = OnceLock::new();

    CACHED
        .get_or_init(|| {
            let output = std::process::Command::new("sysctl")
                .args(["-n", "kern.bootsessionuuid"])
                .output()
                .ok()?;

            if !output.status.success() {
                return None;
            }

            let uuid = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if uuid.is_empty() { None } else { Some(uuid) }
        })
        .clone()
}

// Last written contents, cached to avoid rewriting the file when nothing changed.
static LAST_WRITTEN: Mutex<Option<String>> = Mutex::new(None);

/// Reads the saved session state, if any and if it belongs to the current boot.
pub fn load() -> Option<SessionState> {
    let contents = std::fs::read_to_string(session_path()).ok()?;
    let state: SessionState = serde_json::from_str(&contents).ok()?;

    // A reboot used to throw all of this away, because window ids get reissued
    // and one of them matching again would put a window wherever an unrelated
    // window used to live. But the ids are the only part that goes stale: with
    // "reopen windows when logging back in" ticked, macOS brings the same windows
    // back with the same titles, and those still say where each one belongs.
    //
    // So the session is kept and the unreliable half of it is switched off.
    let mut state = state;
    state.ids_are_current = matches!(boot_uuid(), Some(current) if current == state.boot_uuid);

    if !state.ids_are_current {
        tracing::info!("session is from a previous boot: matching windows by name and title");
    }

    Some(state)
}

/// Builds the current state and writes it to disk (only if it changed).
pub fn save(wm: &WindowManager) {
    let state = build(wm);

    let json = match serde_json::to_string_pretty(&state) {
        Ok(json) => json,
        Err(error) => {
            tracing::warn!("could not serialize session state: {error}");
            return;
        }
    };

    let mut last = match LAST_WRITTEN.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };

    if last.as_deref() == Some(json.as_str()) {
        return;
    }

    if let Err(error) = std::fs::write(session_path(), &json) {
        tracing::warn!("could not write session state: {error}");
        return;
    }

    *last = Some(json);
}

fn build(wm: &WindowManager) -> SessionState {
    let mut windows = Vec::new();
    let boot_uuid = boot_uuid().unwrap_or_default();

    for (m_idx, monitor) in wm.monitors.elements().iter().enumerate() {
        for (w_idx, workspace) in monitor.workspaces().iter().enumerate() {
            for (c_idx, container) in workspace.containers().iter().enumerate() {
                for window in container.windows() {
                    windows.push(SessionWindow {
                        window_id: window.id,
                        exe: window.exe().unwrap_or_default(),
                        title: window.title().unwrap_or_default(),
                        monitor: m_idx,
                        workspace: w_idx,
                        index: c_idx,
                    });
                }
            }

            for window in workspace.floating_windows() {
                windows.push(SessionWindow {
                    window_id: window.id,
                    exe: window.exe().unwrap_or_default(),
                    title: window.title().unwrap_or_default(),
                    monitor: m_idx,
                    workspace: w_idx,
                    // Floating windows are not in the grid, so they have no slot
                    // to come back to.
                    index: 0,
                });
            }
        }
    }

    SessionState {
        boot_uuid,
        windows,
        ids_are_current: true,
    }
}
