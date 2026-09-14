#![deny(clippy::unwrap_used, clippy::expect_used)]

//! Focus whatever window the cursor is over.
//!
//! # Why polling rather than the event tap
//!
//! There is already a `CGEventTap` in [`crate::input_event_listener`], and adding
//! `MouseMoved` to its mask is one bit. It is the wrong place. That tap is an active one,
//! so every event it is subscribed to passes through komorebi **while the system waits**;
//! mouse movement arrives up to 120 times a second, and its callback notifies the reaper
//! on every single event it sees. A callback that takes too long gets the tap disabled by
//! macOS, which would take window dragging, resizing and reaping down with it.
//!
//! Reading the cursor instead costs a local call and can neither stall input nor be
//! switched off by the system.
//!
//! # Why the window under the cursor is free to find
//!
//! `Workspace::latest_layout` already holds the rectangle of every container, because
//! that is what placed the windows there. Finding the one under the cursor is arithmetic:
//! no Accessibility calls, no window server queries.
//!
//! # Moving onto a window, not merely being over one
//!
//! The first version focused whatever the cursor was over, every tenth of a second, and
//! that is a different feature: it does not follow the mouse, it **overrules everything
//! else**. Typing into a launcher stopped working -- the cursor was resting over some
//! other window, so focus was pulled off the launcher mid-word and the search never ran.
//! Bringing a window to the front from anywhere else had the same fate: whatever the
//! cursor happened to be over took focus straight back, a tenth of a second later.
//!
//! Focus following the mouse means the cursor **entering** a window focuses it. A cursor
//! that has not moved is not asking for anything, so every other way of choosing a window
//! -- a launcher, Cmd+Tab, changing workspace -- keeps what it chose until the mouse
//! actually goes somewhere.
//!
//! # Slow applications sort themselves out
//!
//! Focusing is not uniformly cheap: measured over 1358 of them, 4ms median and 16ms at
//! the 90th percentile, but a long tail -- 13 above 50ms and two at about a second, spent
//! entirely inside `activateWithOptions`, which runs in the other application's process
//! and cannot be called back.
//!
//! It needs no cancelling machinery. This loop focuses in line, so a slow application
//! simply holds it up; when it returns it reads the cursor **afresh** and acts on where
//! the cursor is now. Windows crossed while it was blocked are never sampled, so they are
//! never focused, and no backlog builds up. Moving on during a slow focus lands on the
//! right window by construction.

use crate::macos_api::MacosApi;
use crate::window::Window;
use crate::window_manager::WindowManager;
use parking_lot::Mutex;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

/// How often the cursor is read.
///
/// Ten times a second: below what anyone notices when moving a pointer, and far below
/// what a local read costs anything to answer.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Whether the cursor decides focus at all.
///
/// On by default for now. There is a place reserved for this in the configuration --
/// upstream's `focus_follows_mouse` scaffolding is still in the tree, commented out --
/// and once it is wired up the default belongs there rather than here.
static ENABLED: AtomicBool = AtomicBool::new(true);

pub fn is_enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

pub fn set_enabled(enabled: bool) {
    tracing::warn!("FOLLOWMOUSE {}", if enabled { "on" } else { "off" });
    ENABLED.store(enabled, Ordering::Relaxed);
}

/// Whether the window in front is one komorebi has a say over.
///
/// A launcher, System Settings, a save dialog: the user is in it, and the mouse drifting
/// over a window behind it is not a reason to throw them out of it. An id of 0 means the
/// foreground could not be read, which is evidence of nothing, so it is left alone too.
fn foreground_is_ours(wm: &WindowManager, foreground: u32) -> bool {
    foreground == 0
        || wm.monitors().iter().any(|monitor| {
            monitor.workspaces().iter().any(|workspace| {
                workspace.contains_window(foreground)
                    || workspace
                        .floating_windows()
                        .iter()
                        .any(|window| window.id == foreground)
            })
        })
}

/// Where the cursor is, and the window that should have focus because of it.
///
/// Returns the monitor and container it sits in alongside the window, because focus is
/// not just an application being activated: komorebi's own idea of what is selected has
/// to move with it, or the next command acts on whatever was selected before.
fn window_under_cursor(wm: &WindowManager, x: i32, y: i32) -> Option<(usize, usize, Window)> {
    for (monitor_idx, monitor) in wm.monitors().iter().enumerate() {
        // Not `?`: a monitor with nothing focused is a reason to look at the next one,
        // not to give up on the ones after it.
        let Some(workspace) = monitor.focused_workspace() else {
            continue;
        };

        // A window held in monocle covers the whole workspace, and the windows underneath
        // it keep their rectangles in the layout -- so the cursor would "touch" windows
        // that are not on screen and focus would jump to something invisible. While one
        // is up, the cursor has nothing to say.
        if workspace.monocle_container.is_some() {
            continue;
        }

        for (container_idx, container) in workspace.containers().iter().enumerate() {
            let Some(rect) = workspace.latest_layout.get(container_idx) else {
                continue;
            };

            // `right` and `bottom` are the width and the height, not the far edges.
            let inside = x >= rect.left
                && x < rect.left + rect.right
                && y >= rect.top
                && y < rect.top + rect.bottom;

            if inside && let Some(window) = container.focused_window() {
                return Some((monitor_idx, container_idx, window.clone()));
            }
        }
    }

    None
}

pub fn listen(wm: Arc<Mutex<WindowManager>>) {
    std::thread::spawn(move || {
        // Answering the pointer is worth doing promptly, but it is never the thing the
        // user is waiting on the way a command they just typed is.
        crate::qos::set_for_current_thread(crate::qos::QosClass::Utility);

        let mut last_point: Option<(i32, i32)> = None;
        let mut last_window: Option<u32> = None;

        loop {
            std::thread::sleep(POLL_INTERVAL);

            if !is_enabled() {
                last_point = None;
                last_window = None;
                continue;
            }

            let cursor = MacosApi::cursor_pos();
            let point = (cursor.x as i32, cursor.y as i32);
            let moved = last_point.is_some_and(|previous| previous != point);
            last_point = Some(point);

            // Sampled on every pass, moved or not.
            //
            // Skipping this while the cursor is still was a bug of its own: the world
            // changes underneath a motionless cursor -- changing workspace puts an
            // entirely different window beneath it -- and the note left over from before
            // then read as the cursor entering somewhere it had in fact been sitting all
            // along. The next twitch of the mouse pulled focus off whatever had just been
            // chosen. Sampling always keeps the note describing what is really there.
            let under = {
                let wm = wm.lock();

                if wm.is_paused {
                    None
                } else {
                    window_under_cursor(&wm, point.0, point.1)
                }
            };

            let under_id = under.as_ref().map(|(_, _, window)| window.id);

            // Entering a window is the whole trigger: the cursor went somewhere, and
            // somewhere is not where it was. A still cursor is not asking for anything,
            // which is what leaves every other way of choosing a window -- a command, a
            // launcher, Cmd+Tab -- holding on to what it chose.
            let entered = moved && under_id.is_some() && under_id != last_window;
            last_window = under_id;

            if !entered {
                continue;
            }

            let foreground = MacosApi::foreground_window_id().unwrap_or_default();

            let Some((monitor_idx, container_idx, window)) = under else {
                continue;
            };

            {
                let mut wm = wm.lock();

                if !foreground_is_ours(&wm, foreground) {
                    continue;
                }

                wm.focus_monitor(monitor_idx).ok();

                if let Ok(workspace) = wm.focused_workspace_mut() {
                    workspace.focus_container(container_idx);
                }
            }

            // TRACE: every focus komorebi performs looks alike in the log, so without
            // this there is no telling one the cursor asked for from one a command did.
            // Grep marker: FOLLOWMOUSE entered.
            tracing::warn!(
                "FOLLOWMOUSE entered {} ({:?}) at {},{}",
                window.id,
                window.exe().unwrap_or_default(),
                point.0,
                point.1
            );

            // Never the configured value: with `mouse_follows_focus` on, focusing would
            // warp the cursor, the warped cursor would pick a window, and that window
            // would warp the cursor again.
            if let Err(error) = window.focus(false) {
                tracing::warn!("could not focus {} under the cursor: {error}", window.id);
            }
        }
    });
}
