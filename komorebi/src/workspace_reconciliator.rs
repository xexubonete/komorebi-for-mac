#![deny(clippy::unwrap_used, clippy::expect_used)]

use crate::window_manager::WindowManager;
use crate::window_manager_event::WindowManagerEvent;
use crossbeam_channel::Receiver;
use crossbeam_channel::Sender;
use parking_lot::Mutex;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

#[derive(Copy, Clone, Debug)]
pub struct Notification {
    pub monitor_idx: usize,
    pub workspace_idx: usize,
    pub triggered_by: WindowManagerEvent,
    /// Which user-navigation generation this was raised in. See [`USER_WORKSPACE_GENERATION`].
    pub generation: u64,
}

/// Bumped every time the user changes workspace themselves.
///
/// Reconciliation carries a fixed destination and is acted on some time after it was
/// raised. Changing workspace quickly -- 3 to 2 to 1 -- leaves a request for workspace 2
/// in flight, and by the time it runs the user is on 1 and gets pulled back. Comparing
/// generations tells a request that still describes where the user is from one that has
/// been overtaken.
pub static USER_WORKSPACE_GENERATION: AtomicU64 = AtomicU64::new(0);

/// The window komorebi focused itself, most recently.
///
/// Focusing a window makes macOS report a focus change, which arrives indistinguishable
/// from the user clicking on it. Changing workspace focuses the window waiting there, so
/// moving 3 -> 2 -> 1 quickly leaves komorebi reacting to its own focus changes for
/// workspaces the user is passing through -- and reconciling back to one of them.
///
/// This used to be a queue with the record removed on the first match, and that was the
/// bug: **one focus produces several reports**, not one. Focusing a window makes macOS
/// send AXFocusedWindowChanged, then AXApplicationActivated, then AXMainWindowChanged,
/// then NSWorkspaceDidActivateApplication. The first was recognised as komorebi's own and
/// took the record with it; the rest arrived to find nothing and were taken for the user
/// changing windows. Measured: every stray reconciliation was triggered by one of the
/// later three, never by the first.
///
/// So a record is never consumed by the report that matches it. It has to survive two
/// things at once, and getting either wrong brings the bug back:
///
/// * **The whole burst for one window.** Removing the record on the first report left the
///   other three unrecognised.
/// * **The bursts of several windows at the same time.** Navigating 1 -> 2 quickly means
///   komorebi focuses a window on each, and the reports for the first arrive around 400ms
///   later, well after it has focused the second. A single slot is overwritten by then,
///   and the late report for the first window looks like the user asking to go back to
///   where it lives. Measured: exactly this, with the reconciliation landing on the
///   workspace the user had passed through.
///
/// Hence a short history rather than one slot. Eight covers far more rapid navigation
/// than any burst outlives, and the oldest entry falls off the end.
static FOCUS_WE_CAUSED: Mutex<Vec<u32>> = Mutex::new(Vec::new());

const FOCUS_HISTORY: usize = 8;

/// Record that komorebi is about to focus this window itself.
pub fn note_focus_we_caused(window_id: u32) {
    let mut ours = FOCUS_WE_CAUSED.lock();

    // Already the most recent? Then re-recording it would push out an older entry that is
    // still waiting for its reports to arrive.
    if ours.last() == Some(&window_id) {
        return;
    }

    ours.retain(|id| *id != window_id);
    ours.push(window_id);

    if ours.len() > FOCUS_HISTORY {
        ours.remove(0);
    }
}

/// Whether this focus change is one komorebi caused.
pub fn focus_was_ours(window_id: u32) -> bool {
    FOCUS_WE_CAUSED.lock().contains(&window_id)
}


/// Record that the user navigated to a workspace directly.
pub fn note_user_changed_workspace() {
    USER_WORKSPACE_GENERATION.fetch_add(1, Ordering::SeqCst);
}



static RECONCILIATION_IN_PROGRESS: AtomicBool = AtomicBool::new(false);
static LAST_RECONCILIATION: AtomicU64 = AtomicU64::new(0);
const COOLDOWN_MS: u64 = 1000; // 1 second cooldown

static CHANNEL: OnceLock<(Sender<Notification>, Receiver<Notification>)> = OnceLock::new();

pub fn channel() -> &'static (Sender<Notification>, Receiver<Notification>) {
    CHANNEL.get_or_init(|| crossbeam_channel::bounded(1))
}

fn event_tx() -> Sender<Notification> {
    channel().0.clone()
}

fn event_rx() -> Receiver<Notification> {
    channel().1.clone()
}

pub fn send_notification(
    monitor_idx: usize,
    workspace_idx: usize,
    triggered_by: WindowManagerEvent,
) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    if now - LAST_RECONCILIATION.load(Ordering::SeqCst) < COOLDOWN_MS {
        tracing::debug!("within cooldown period, dropping notification");
        return;
    }

    if !RECONCILIATION_IN_PROGRESS.load(Ordering::Relaxed) {
        // TRACE: reconciliation is the only thing that changes workspace without the user
        // asking, so when the workspace ends up somewhere unexpected this is the first
        // place to look. Grep marker: RECONCILE.
        tracing::warn!(
            "RECONCILE raised monitor={monitor_idx} workspace={workspace_idx} generation={} trigger={triggered_by:?}",
            USER_WORKSPACE_GENERATION.load(Ordering::SeqCst)
        );
        if event_tx()
            .try_send(Notification {
                monitor_idx,
                workspace_idx,
                triggered_by,
                generation: USER_WORKSPACE_GENERATION.load(Ordering::SeqCst),
            })
            .is_err()
        {
            tracing::warn!("channel is full; dropping notification")
        }
    }
}

pub fn listen_for_notifications(wm: Arc<Mutex<WindowManager>>) {
    std::thread::spawn(move || {
        // Reconciliation ends up laying out a workspace, which the user is looking at,
        // but it is always reacting rather than answering something they just asked for.
        crate::qos::set_for_current_thread(crate::qos::QosClass::UserInitiated);

        loop {
            match handle_notifications(wm.clone()) {
                Ok(()) => {
                    tracing::warn!("restarting finished thread");
                }
                Err(error) => {
                    if cfg!(debug_assertions) {
                        tracing::error!("restarting failed thread: {:?}", error)
                    } else {
                        tracing::error!("restarting failed thread: {}", error)
                    }
                }
            }
        }
    });
}
#[tracing::instrument(skip_all)]
pub fn handle_notifications(wm: Arc<Mutex<WindowManager>>) -> color_eyre::Result<()> {
    tracing::info!("listening");

    let receiver = event_rx();

    for notification in &receiver {
        // Overtaken by the user: they have navigated since this was raised, so acting on
        // it would drag them back to a workspace they already left.
        let generation_now = USER_WORKSPACE_GENERATION.load(Ordering::SeqCst);

        if notification.generation != generation_now {
            tracing::warn!(
                "RECONCILE dropped (raised at generation {}, user is now at {generation_now})",
                notification.generation
            );
            continue;
        }


        RECONCILIATION_IN_PROGRESS.store(true, Ordering::Relaxed);
        tracing::info!("running reconciliation for notification {notification:?}");

        let mut wm = wm.lock();
        let focused_monitor_idx = wm.focused_monitor_idx();
        let focused_workspace_idx =
            wm.focused_workspace_idx_for_monitor_idx(focused_monitor_idx)?;

        let focused_pair = (focused_monitor_idx, focused_workspace_idx);
        let updated_pair = (notification.monitor_idx, notification.workspace_idx);

        if focused_pair != updated_pair {
            // don't switch workspaces if the current workspace is empty
            // this happens when the user just closed the last window on a workspace
            // and the last focused application, usually on another workspace, takes focus
            // and in doing so triggers the reconciliator
            if let Ok(workspace) = wm.focused_workspace()
                && workspace.containers().is_empty()
            {
                tracing::debug!(
                    "current workspace is empty (user closed last window), not reconciling to prevent unwanted workspace switch"
                );
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64;
                LAST_RECONCILIATION.store(now, Ordering::SeqCst);
                RECONCILIATION_IN_PROGRESS.store(false, Ordering::Relaxed);
                continue;
            }

            tracing::warn!(
                "RECONCILE acting: {focused_monitor_idx}/{focused_workspace_idx} -> {}/{}",
                notification.monitor_idx,
                notification.workspace_idx
            );
            wm.focus_monitor(notification.monitor_idx)?;
            let mouse_follows_focus = wm.mouse_follows_focus;

            // Did one of our own windows trigger this, or someone else's?
            //
            // Reconciliation runs off system events, not off anything the user asked
            // komorebi to do, so it has to be careful about taking focus. A window we
            // manage coming to the front (Cmd+Tab) should end up focused. A window we do
            // not manage coming to the front -- System Settings, opened from a launcher
            // -- is the user going somewhere else, and stealing focus back drags them out
            // of the window that just appeared.
            let triggered_by_managed_window = notification
                .triggered_by
                .window_id()
                .is_some_and(|window_id| {
                    wm.monitors().iter().any(|monitor| {
                        monitor
                            .workspaces()
                            .iter()
                            .any(|workspace| workspace.contains_window(window_id))
                    })
                });

            if let Some(monitor) = wm.focused_monitor_mut() {
                let previous_idx = monitor.focused_workspace_idx();
                monitor.last_focused_workspace = Option::from(previous_idx);
                monitor.focus_workspace(notification.workspace_idx)?;

                // The workspace still needs laying out either way; the only question is
                // whether to grab focus at the end of it.
                if triggered_by_managed_window {
                    monitor.load_focused_workspace(mouse_follows_focus)?;
                } else {
                    // This is the branch that changes workspace and focuses nothing --
                    // exactly what "it jumped somewhere and no window has focus" looks
                    // like from the outside.
                    tracing::warn!(
                        "RECONCILE trigger is not a window komorebi manages; switching without taking focus"
                    );
                    monitor.load_focused_workspace_without_taking_focus(mouse_follows_focus)?;
                }
            }

            if let Some(window_id) = notification.triggered_by.window_id() {
                // Only hand focus over when the window that triggered this is one we
                // actually manage.
                //
                // The point of focusing here is Cmd+Tab: the window being switched to
                // lives on another space, and it should end up genuinely focused rather
                // than leaving focus on whichever container was selected before. But a
                // window komorebi does not manage can trigger this too -- System Settings
                // opening is one -- and then focus_container_by_window finds nothing,
                // focused_container is still whatever was selected a moment ago, and we
                // would pull focus onto that instead. The window the user just opened
                // loses focus roughly 25ms after appearing.
                //
                // So the search result decides: found means the trigger is ours and
                // focusing it is right; not found means someone else's window is coming
                // to the front and it is not our business to interfere.
                let manages_trigger = triggered_by_managed_window
                    && wm
                        .focused_workspace_mut()
                        .is_ok_and(|workspace| workspace.focus_container_by_window(window_id).is_ok());

                if manages_trigger
                    && let Ok(workspace) = wm.focused_workspace()
                    && let Some(container) = workspace.focused_container()
                    && let Some(window) = container.focused_window()
                {
                    let _ = window.focus(mouse_follows_focus);
                }
            }

            crate::border_manager::event_tx()
                .try_send(crate::border_manager::Notification::ForceUpdate)
                .ok();
        }

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        LAST_RECONCILIATION.store(now, Ordering::SeqCst);
        RECONCILIATION_IN_PROGRESS.store(false, Ordering::Relaxed);
    }

    Ok(())
}
