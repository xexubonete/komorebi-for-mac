mod border;
mod ns_window;

use crate::AccessibilityUiElement;
use crate::CoreFoundationRunLoop;
use crate::accessibility::AccessibilityApi;
use crate::border_manager::border::Border;
pub use crate::border_manager::ns_window::FLASH_DURATION_MS;
pub use crate::border_manager::ns_window::FLASH_FACTOR_X10;
pub use crate::border_manager::ns_window::FlashStyle;
pub use crate::border_manager::ns_window::set_flash_easing;
use crate::core::WindowKind;
use crate::macos_api::MacosApi;
use crate::ring::Ring;
use crate::window_manager::WindowManager;
use crate::workspace::Workspace;
use crate::workspace::WorkspaceLayer;
use color_eyre::eyre;
use crossbeam_channel::Receiver;
use crossbeam_channel::RecvTimeoutError;
use crossbeam_channel::Sender;
use crossbeam_utils::atomic::AtomicConsume;
use dispatch2::DispatchQueue;
use komorebi_themes::colour::Colour;
use komorebi_themes::colour::Rgb;
use lazy_static::lazy_static;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicI32;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

pub static BORDER_WIDTH: AtomicI32 = AtomicI32::new(10);
pub static BORDER_OFFSET: AtomicI32 = AtomicI32::new(0);
pub static BORDER_RADIUS: AtomicI32 = AtomicI32::new(10);
pub static BORDER_ENABLED: AtomicBool = AtomicBool::new(true);

// Windows has a 7px invisible border around every app which the border_x
// config options have to take into account. In order to keep the komorebi
// configuration truly portable across Windows and macOS, we need to add
// that same 7px adjustment to whatever BORDER_OFFSET is set to. I hate this,
// but config compatibility has to come first, and komorebi for Windows already
// has tens of thousands of users.
pub static BORDER_OFFSET_ADJUSTMENT: i32 = 7;

/// How long the border manager waits for a notification before checking the foreground
/// window on its own.
///
/// Short enough that the borders get out of the way before the eye registers it, and
/// cheap enough to ignore: five Accessibility reads a second, and only while a border is
/// actually on screen.
const FOREGROUND_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(200);

/// How long nothing may hold the foreground before komorebi treats it as a window having
/// closed, rather than focus being handed from one window to another.
///
/// A handover leaves this gap for a few milliseconds: a launcher quits while the app it
/// launched is still coming up. A closed window leaves it until something else is
/// focused, which may be never. Only the second case wants focus restored, and the only
/// thing telling them apart is how long the gap lasts -- counting how many times it was
/// observed does not, because several notifications can arrive within the same
/// millisecond of a single handover.
const FOREGROUND_LOST_GRACE: std::time::Duration = std::time::Duration::from_millis(400);

/// Set whenever a window appears, from anywhere in komorebi.
///
/// A window that has just opened is about to be given focus by macOS. Anything here that
/// would otherwise pull focus elsewhere has to stand down while that is happening, or the
/// window the user just opened comes up already unfocused.
static LAST_WINDOW_APPEARED: AtomicU64 = AtomicU64::new(0);

/// How long after a window appears komorebi keeps its hands off the focus.
///
/// Long enough to cover an application launching and putting its first window up, which
/// is where the race is.
const WINDOW_APPEARANCE_GRACE: std::time::Duration = std::time::Duration::from_millis(1500);

/// Record that a window just appeared. See [`LAST_WINDOW_APPEARED`].
pub fn note_window_appeared() {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    LAST_WINDOW_APPEARED.store(now, Ordering::Relaxed);
}

/// Whether a window appeared recently enough that focus should be left alone.
fn window_appeared_recently() -> bool {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    now.saturating_sub(LAST_WINDOW_APPEARED.load(Ordering::Relaxed))
        < WINDOW_APPEARANCE_GRACE.as_millis() as u64
}

lazy_static! {
    pub static ref FOCUSED: AtomicU32 =
        AtomicU32::new(u32::from(Colour::Rgb(Rgb::new(66, 165, 245))));
    pub static ref UNFOCUSED: AtomicU32 =
        AtomicU32::new(u32::from(Colour::Rgb(Rgb::new(128, 128, 128))));
    pub static ref UNFOCUSED_LOCKED: AtomicU32 =
        AtomicU32::new(u32::from(Colour::Rgb(Rgb::new(158, 8, 8))));
    pub static ref MONOCLE: AtomicU32 =
        AtomicU32::new(u32::from(Colour::Rgb(Rgb::new(255, 51, 153))));
    pub static ref STACK: AtomicU32 = AtomicU32::new(u32::from(Colour::Rgb(Rgb::new(0, 165, 66))));
    pub static ref FLOATING: AtomicU32 =
        AtomicU32::new(u32::from(Colour::Rgb(Rgb::new(245, 245, 165))));
}

lazy_static! {
    /// Per-application corner radius overrides. See `border_radius_rules` in the config.
    static ref BORDER_RADIUS_RULES: Mutex<HashMap<String, i32>> = Mutex::new(HashMap::new());
}

/// Which animation the border plays when its window takes focus.
static FLASH_STYLE: Mutex<FlashStyle> = Mutex::new(FlashStyle::Width);

/// Set the focus animation style, from config load or reload.
pub fn set_flash_style(style: FlashStyle) {
    tracing::info!("border focus animation: {style:?}");
    *FLASH_STYLE.lock() = style;
}

/// The focus animation style currently in force.
pub fn flash_style() -> FlashStyle {
    *FLASH_STYLE.lock()
}

/// Replace the per-application radius overrides, from config load or reload.
pub fn set_border_radius_rules(rules: HashMap<String, i32>) {
    tracing::info!("loaded border radius rules for {} applications", rules.len());
    *BORDER_RADIUS_RULES.lock() = rules;
}

/// The corner radius to draw for a window belonging to this application.
///
/// Falls back to the global radius, which is right for anything using the system
/// window frame.
pub fn border_radius_for(application: &str) -> i32 {
    BORDER_RADIUS_RULES
        .lock()
        .get(application)
        .copied()
        .unwrap_or_else(|| BORDER_RADIUS.load(Ordering::Relaxed))
}

lazy_static! {
    static ref BORDER_STATE: Mutex<HashMap<String, Box<Border>>> = Mutex::new(HashMap::new());
    static ref WINDOWS_BORDERS: Mutex<HashMap<u32, String>> = Mutex::new(HashMap::new());
}

pub enum Notification {
    // NOTE: bool should only ever be set to true by the reaper
    // TODO: eventually the bool should be a source enum so we can match on emitters
    Update(Option<AccessibilityUiElement>, Option<u32>, bool),
    ForceUpdate,
}

static CHANNEL: OnceLock<(Sender<Notification>, Receiver<Notification>)> = OnceLock::new();

pub fn channel() -> &'static (Sender<Notification>, Receiver<Notification>) {
    CHANNEL.get_or_init(|| crossbeam_channel::bounded(50))
}

pub fn event_tx() -> Sender<Notification> {
    channel().0.clone()
}

fn event_rx() -> Receiver<Notification> {
    channel().1.clone()
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct BorderInfo {
    pub border_id: String,
    pub window_kind: WindowKind,
}

pub fn window_border(window_id: u32) -> Option<BorderInfo> {
    let id = WINDOWS_BORDERS.lock().get(&window_id)?.clone();
    BORDER_STATE.lock().get(&id).map(|b| BorderInfo {
        border_id: b.id.clone(),
        window_kind: b.window_kind,
    })
}

pub fn send_notification(
    element: Option<AccessibilityUiElement>,
    window_id: Option<u32>,
    reaper: bool,
) {
    if event_tx()
        .try_send(Notification::Update(element, window_id, reaper))
        .is_err()
    {
        tracing::warn!("channel is full; dropping notification")
    }
}

pub fn destroy_all_borders() -> eyre::Result<()> {
    let mut borders = BORDER_STATE.lock();
    tracing::info!(
        "purging known borders: {:?}",
        borders.iter().map(|b| b.1.id.clone()).collect::<Vec<_>>()
    );

    for (_, border) in borders.drain() {
        let _ = destroy_border(border);
    }

    drop(borders);

    WINDOWS_BORDERS.lock().clear();

    Ok(())
}

fn window_kind_colour(focus_kind: WindowKind) -> u32 {
    match focus_kind {
        WindowKind::Unfocused => UNFOCUSED.load(Ordering::Relaxed),
        WindowKind::UnfocusedLocked => UNFOCUSED_LOCKED.load(Ordering::Relaxed),
        WindowKind::Single => FOCUSED.load(Ordering::Relaxed),
        WindowKind::Stack => STACK.load(Ordering::Relaxed),
        WindowKind::Monocle => MONOCLE.load(Ordering::Relaxed),
        WindowKind::Floating => FLOATING.load(Ordering::Relaxed),
    }
}

fn remove_borders(
    borders: &mut HashMap<String, Box<Border>>,
    windows_borders: &mut HashMap<u32, String>,
    monitor_idx: usize,
    condition: impl Fn(&String, &Border) -> bool,
) -> color_eyre::Result<()> {
    let mut to_remove = vec![];
    for (id, border) in borders.iter() {
        // if border is on this monitor
        if border.monitor_idx.is_some_and(|idx| idx == monitor_idx)
            // and the condition applies
            && condition(id, border)
        {
            // we mark it to be removed
            to_remove.push(id.clone());
        }
    }

    for id in &to_remove {
        remove_border(id, borders, windows_borders)?;
    }

    Ok(())
}

fn remove_border(
    id: &str,
    borders: &mut HashMap<String, Box<Border>>,
    windows_borders: &mut HashMap<u32, String>,
) -> color_eyre::Result<()> {
    if let Some(removed_border) = borders.remove(id) {
        windows_borders.remove(&removed_border.tracking_window_id);
        destroy_border(removed_border)?;
    }

    Ok(())
}

fn destroy_border(border: Box<Border>) -> color_eyre::Result<()> {
    DispatchQueue::main().exec_sync(|| {
        tracing::info!("invalidating border observer");
        AccessibilityApi::invalidate_observer(&border.observer);
    });

    std::thread::sleep(std::time::Duration::from_millis(10));

    let raw_pointer = Box::into_raw(border);
    unsafe {
        (*raw_pointer).destroy();
    }

    Ok(())
}

pub fn listen_for_notifications(wm: Arc<Mutex<WindowManager>>, run_loop: CoreFoundationRunLoop) {
    std::thread::spawn(move || {
        loop {
            match handle_notifications(wm.clone(), run_loop.clone()) {
                Ok(()) => {
                    tracing::warn!("restarting finished thread");
                }
                Err(error) => {
                    tracing::warn!("restarting failed thread: {}", error);
                }
            }
        }
    });
}

fn handle_notifications(
    wm: Arc<Mutex<WindowManager>>,
    run_loop: CoreFoundationRunLoop,
) -> color_eyre::Result<()> {
    tracing::info!("listening");

    let receiver = event_rx();
    event_tx().send(Notification::Update(None, None, false))?;

    let mut previous_snapshot = Ring::default();
    let mut previous_pending_move_op = None;
    let mut previous_is_paused = false;
    let mut previous_notification: Option<Notification> = None;
    let mut previous_layer = WorkspaceLayer::default();
    let mut previous_foreground_is_managed = true;
    let mut previous_foreground_window = 0u32;
    let mut foreground_lost_since: Option<std::time::Instant> = None;

    'receiver: loop {
        // Wait for a notification, but not indefinitely.
        //
        // Everything here used to run only when komorebi sent word, and komorebi only
        // sends word after handling an event of its own windows. Focus moving to a
        // window it does not manage produces no such event -- opening System Settings
        // yielded exactly two, on launch and on quit, and no focus change at all. So
        // whether the borders got out of the way came down to whether some unrelated
        // event happened to arrive just after the focus moved. Sometimes one did;
        // often none did, and the borders stayed drawn across it indefinitely.
        //
        // Waiting with a deadline means that when nothing arrives, the foreground is
        // checked anyway. That is the whole fix: stop waiting to be told about a
        // change that nobody is going to report.
        let notification = match receiver.recv_timeout(FOREGROUND_POLL_INTERVAL) {
            Ok(notification) => Some(notification),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => break 'receiver,
        };

        // On a deadline wake-up, get out as early as possible: everything below takes
        // the window manager lock and clones the whole monitor state, which is far too
        // much to do five times a second for a question that is almost always "no".
        if notification.is_none() {
            // Nothing on screen to hide.
            if BORDER_STATE.lock().is_empty() {
                continue 'receiver;
            }

            // Same window in front as last time, so nothing can have changed. Two
            // Accessibility reads, and that is the whole cost of the poll.
            let foreground = MacosApi::foreground_window_id().unwrap_or_default();

            if foreground == previous_foreground_window {
                continue 'receiver;
            }
        }

        let state = wm.lock();
        let is_paused = state.is_paused;
        let focused_monitor_idx = state.focused_monitor_idx();
        let focused_workspace_idx =
            state.monitors.elements()[focused_monitor_idx].focused_workspace_idx();
        let monitors = state.monitors.clone();
        let pending_move_op = *state.pending_move_op;
        let floating_window_hwnds = state.monitors.elements()[focused_monitor_idx].workspaces()
            [focused_workspace_idx]
            .floating_windows()
            .iter()
            .map(|w| w.id)
            .collect::<Vec<_>>();
        let workspace_layer = state.monitors.elements()[focused_monitor_idx].workspaces()
            [focused_workspace_idx]
            .layer;
        let foreground_window = MacosApi::foreground_window_id().unwrap_or_default();

        // Is the window in front one of ours?
        //
        // A border floats above every ordinary window and cannot be slotted in between
        // (see NsWindow::new), so while the user is working in something komorebi does
        // not manage -- System Settings, a save dialog -- a border left showing is just
        // a stripe drawn across it. komorebi has no say over that window's stacking, but
        // it does get to decide whether to draw anything at all, so it draws nothing.
        //
        // An id of 0 means the foreground window could not be determined; treat that as
        // ours so an unlucky reading never blanks the borders.
        let foreground_is_managed = foreground_window == 0
            || monitors.elements().iter().any(|monitor| {
                monitor.workspaces().iter().any(|workspace| {
                    workspace.contains_window(foreground_window)
                        || workspace
                            .floating_windows()
                            .iter()
                            .any(|window| window.id == foreground_window)
                })
            });

        // Track how long the foreground has been unattributable, so a handover can be
        // told from a window that closed.
        if foreground_window == 0 {
            foreground_lost_since.get_or_insert_with(std::time::Instant::now);
        } else {
            foreground_lost_since = None;
        }

        let foreground_lost_long_enough = foreground_lost_since
            .is_some_and(|since| since.elapsed() >= FOREGROUND_LOST_GRACE);

        // DIAGNOSTIC: which window the border manager believes is in front, and whether
        // it counts as one of ours. Grep marker: FOREGROUND.
        if foreground_is_managed != previous_foreground_is_managed {
            tracing::info!(
                "FOREGROUND changed: window={foreground_window} managed={foreground_is_managed}"
            );
        }

        let _layer_changed = previous_layer != workspace_layer;
        let _forced_update = matches!(notification, Some(Notification::ForceUpdate));

        drop(state);

        let should_process_notification = match notification {
            // Woken by the deadline rather than by a notification. Nothing has been
            // reported, so the only thing worth acting on is the foreground having
            // changed underneath us -- which is exactly the case nobody reports.
            None => foreground_is_managed != previous_foreground_is_managed,
            Some(Notification::Update(_, notification_window_id, reaper)) => {
                let mut should_process_notification = true;

                if monitors == previous_snapshot
                    // handle the window dragging edge case
                    && pending_move_op == previous_pending_move_op
                {
                    should_process_notification = false;
                }

                // handle the pause edge case
                if is_paused && !previous_is_paused {
                    should_process_notification = true;
                }

                // handle the unpause edge case
                if previous_is_paused && !is_paused {
                    should_process_notification = true;
                }

                // handle the retile edge case
                if !should_process_notification && BORDER_STATE.lock().is_empty() {
                    should_process_notification = true;
                }

                // when we switch focus to/from a floating window
                let switch_focus_to_from_floating_window = floating_window_hwnds.iter().any(|fw| {
                    // if we switch focus to a floating window
                    fw == &notification_window_id.unwrap_or_default() ||
                            // if there is any floating window with a `WindowKind::Floating` border
                            // that no longer is the foreground window then we need to update that
                            // border.
                            (fw != &foreground_window
                                && window_border(*fw)
                                .is_some_and(|b| b.window_kind == WindowKind::Floating))
                });

                // when the focused window has an `Unfocused` border kind, usually this happens if
                // we focus an admin window and then refocus the previously focused window. For
                // komorebi it will have the same state has before, however the previously focused
                // window changed its border to unfocused so now we need to update it again.
                if !should_process_notification
                    && window_border(notification_window_id.unwrap_or_default())
                        .is_some_and(|b| b.window_kind == WindowKind::Unfocused)
                {
                    should_process_notification = true;
                }

                if !should_process_notification && switch_focus_to_from_floating_window {
                    should_process_notification = true;
                }

                if !should_process_notification
                    && let Some(Notification::Update(_, ref previous_window_id, false)) =
                        previous_notification
                    && previous_window_id.unwrap_or_default()
                        != notification_window_id.unwrap_or_default()
                {
                    should_process_notification = true;
                }

                if reaper {
                    should_process_notification = true;
                }

                // Focus moving to or from a window komorebi does not manage leaves its
                // own state untouched, so the snapshot comparison above sees nothing
                // happening and skips the notification. That is exactly the moment the
                // borders need to come down or go back up.
                if foreground_is_managed != previous_foreground_is_managed {
                    should_process_notification = true;
                }

                should_process_notification
            }
            Some(Notification::ForceUpdate) => true,
        };

        previous_foreground_is_managed = foreground_is_managed;
        previous_foreground_window = foreground_window;

        if !should_process_notification {
            tracing::debug!("monitor state matches latest snapshot, skipping notification");
            continue 'receiver;
        }

        let mut borders = BORDER_STATE.lock();
        let mut windows_borders = WINDOWS_BORDERS.lock();

        // Something we don't manage is in front: hide the borders rather than draw over
        // it. Hidden, not destroyed -- focus comes back and they are shown again by the
        // usual update, with no windows to rebuild.
        if !foreground_is_managed {
            for border in borders.values() {
                border.ns_window.set_visible(false);
            }

            continue 'receiver;
        }

        // If borders are disabled
        if !BORDER_ENABLED.load_consume()
            // Or if the wm is paused
            || is_paused
        {
            // Destroy the borders we know about
            for (_, border) in borders.drain() {
                destroy_border(border)?;
            }

            windows_borders.clear();

            previous_is_paused = is_paused;
            continue 'receiver;
        }

        'monitors: for (monitor_idx, m) in monitors.elements().iter().enumerate() {
            if let Some(ws) = m.focused_workspace() {
                if !ws.tile {
                    remove_borders(&mut borders, &mut windows_borders, monitor_idx, |_, _| true)?;
                    continue 'monitors;
                }

                if let Some(monocle) = &ws.monocle_container
                    && let Some(window) = monocle.focused_window()
                {
                    let focused_window_id =
                        monocle.focused_window().map(|w| w.id).unwrap_or_default();
                    let id = monocle.id.clone();
                    let border = match borders.entry(id.clone()) {
                        Entry::Occupied(entry) => entry.into_mut(),
                        Entry::Vacant(entry) => {
                            if let Ok(border) = Border::create(
                                &monocle.id,
                                window.id,
                                window.application.process_id,
                                window.element.clone(),
                                Some(monitor_idx),
                                run_loop.clone(),
                            ) {
                                entry.insert(border)
                            } else {
                                continue 'monitors;
                            }
                        }
                    };

                    let new_focus_state = if monitor_idx != focused_monitor_idx {
                        WindowKind::Unfocused
                    } else {
                        WindowKind::Monocle
                    };

                    border.window_kind = new_focus_state;

                    // Update the borders tracking_hwnd in case it changed and remove the
                    // old `tracking_hwnd` from `WINDOWS_BORDERS` if needed.
                    if border.tracking_window_id != focused_window_id {
                        if let Some(previous) = windows_borders.get(&border.tracking_window_id) {
                            // Only remove the border from `windows_borders` if it
                            // still corresponds to the same border, if doesn't then
                            // it means it was already updated by another border for
                            // that window and in that case we don't want to remove it.
                            if previous == &id {
                                windows_borders.remove(&border.tracking_window_id);
                            }
                        }
                        border.tracking_window_id = focused_window_id;
                        // Only update tracking element if same process (safe for tabbed apps)
                        if window.application.process_id == border.process_id {
                            let border_ptr =
                                std::ptr::addr_of_mut!(**border).cast::<std::ffi::c_void>();
                            border.update_tracking_element(window.element.clone(), border_ptr);
                        }
                    }

                    // Update the border's monitor idx in case it changed
                    border.monitor_idx = Some(monitor_idx);
                    border.update();

                    windows_borders.insert(focused_window_id, id);

                    let border_id = border.id.clone();

                    if ws.layer == WorkspaceLayer::Floating {
                        handle_floating_borders(
                            &mut borders,
                            &mut windows_borders,
                            ws,
                            monitor_idx,
                            foreground_window,
                            run_loop.clone(),
                        )?;

                        // Remove all borders on this monitor except monocle and floating borders
                        remove_borders(&mut borders, &mut windows_borders, monitor_idx, |_, b| {
                            border_id != b.id
                                && !ws
                                    .floating_windows()
                                    .iter()
                                    .any(|w| w.id == b.tracking_window_id)
                        })?;
                    } else {
                        // Remove all borders on this monitor except monocle
                        remove_borders(&mut borders, &mut windows_borders, monitor_idx, |_, b| {
                            border_id != b.id
                        })?;
                    }
                    continue 'monitors;
                }

                // Collect focused workspace container and floating windows ID's.
                // Floating borders are keyed by the window id as a string, so we
                // must include them too or we'd remove them right after creating them.
                let mut container_and_floating_window_ids = ws
                    .containers()
                    .iter()
                    .map(|c| c.id.clone())
                    .collect::<Vec<_>>();
                container_and_floating_window_ids
                    .extend(ws.floating_windows().iter().map(|w| w.id.to_string()));

                for (idx, c) in ws.containers().iter().enumerate() {
                    if let Some(window) = c.focused_window() {
                        let id = c.id.clone();

                        let border = match borders.entry(id.clone()) {
                            Entry::Occupied(entry) => entry.into_mut(),
                            Entry::Vacant(entry) => {
                                if let Ok(border) = Border::create(
                                    &c.id,
                                    window.id,
                                    window.application.process_id,
                                    window.element.clone(),
                                    Some(monitor_idx),
                                    run_loop.clone(),
                                ) {
                                    entry.insert(border)
                                } else {
                                    continue 'monitors;
                                }
                            }
                        };

                        // this happens if we cmd+w a window, but the app is still active with no windows
                        let ignore_foreground_window = foreground_window == 0;

                        // Nothing has been in front for long enough that a window must have
                        // closed, rather than focus being in transit between two of them --
                        // and no window has just appeared to claim it.
                        //
                        // The appearance check is what actually settles this: macOS gives a
                        // new window focus by itself, so the only thing komorebi has to do is
                        // stay out of the way while that happens. Waiting on a timer alone
                        // could not tell "nobody has focus because a window closed" from
                        // "nobody has focus yet because one is still opening".
                        let lost_foreground_window =
                            foreground_lost_long_enough && !window_appeared_recently();

                        let focus_state_condition = if ignore_foreground_window {
                            idx != ws.focused_container_idx() || monitor_idx != focused_monitor_idx
                        } else {
                            idx != ws.focused_container_idx()
                                || monitor_idx != focused_monitor_idx
                                || window.id != foreground_window
                        };

                        let new_focus_state = if focus_state_condition {
                            if c.locked {
                                WindowKind::UnfocusedLocked
                            } else {
                                WindowKind::Unfocused
                            }
                        } else if c.windows().len() > 1 {
                            WindowKind::Stack
                        } else {
                            WindowKind::Single
                        };

                        // if we are in the cmd+w situation, we wanna make sure the window we're gonna
                        // set a focused border on actually has keyboard focus
                        //
                        // Only when nothing has held focus for a while, though. "No foreground
                        // window" also happens for a moment during an ordinary handover: a
                        // launcher closing while the app it launched is still coming up leaves
                        // that gap, and forcing focus into it drags the user back to the window
                        // they were leaving -- the window they just opened appears already
                        // unfocused. Requiring the gap to persist across a poll separates a
                        // window that really did close from one that is merely mid-handover,
                        // at the cost of restoring focus a poll later after Cmd+W.
                        if lost_foreground_window
                            && matches!(new_focus_state, WindowKind::Single | WindowKind::Stack)
                        {
                            window.focus(false)?;
                        }

                        border.window_kind = new_focus_state;

                        if border.tracking_window_id != window.id {
                            if let Some(previous) = windows_borders.get(&border.tracking_window_id)
                            {
                                // Only remove the border from `windows_borders` if it
                                // still corresponds to the same border, if doesn't then
                                // it means it was already updated by another border for
                                // that window and in that case we don't want to remove it.
                                if previous == &id {
                                    windows_borders.remove(&border.tracking_window_id);
                                }
                            }
                            border.tracking_window_id = window.id;
                            // Only update tracking element if same process (safe for tabbed apps)
                            if window.application.process_id == border.process_id {
                                let border_ptr =
                                    std::ptr::addr_of_mut!(**border).cast::<std::ffi::c_void>();
                                border.update_tracking_element(window.element.clone(), border_ptr);
                            }
                        }

                        border.monitor_idx = Some(monitor_idx);
                        border.update();

                        windows_borders.insert(window.id, id);
                    }

                    handle_floating_borders(
                        &mut borders,
                        &mut windows_borders,
                        ws,
                        monitor_idx,
                        foreground_window,
                        run_loop.clone(),
                    )?;
                }

                // Remove stale borders (from other workspaces) AFTER creating/
                // updating the current workspace's ones. destroy_border sleeps
                // 10ms per border; doing it first would delay the new borders
                // from appearing after a workspace switch.
                remove_borders(&mut borders, &mut windows_borders, monitor_idx, |id, _| {
                    !container_and_floating_window_ids.contains(id)
                })?;
            }
        }

        previous_snapshot = monitors;
        previous_pending_move_op = pending_move_op;
        previous_is_paused = is_paused;
        // Only remember real notifications: a deadline waking us up is not something to
        // compare the next one against.
        if notification.is_some() {
            previous_notification = notification;
        }
        previous_layer = workspace_layer;
    }

    Ok(())
}

fn handle_floating_borders(
    borders: &mut HashMap<String, Box<Border>>,
    windows_borders: &mut HashMap<u32, String>,
    ws: &Workspace,
    monitor_idx: usize,
    foreground_window: u32,
    run_loop: CoreFoundationRunLoop,
) -> color_eyre::Result<()> {
    for window in ws.floating_windows() {
        let id = window.id.to_string();
        let border = match borders.entry(id.clone()) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => {
                if let Ok(border) = Border::create(
                    &window.id.to_string(),
                    window.id,
                    window.application.process_id,
                    window.element.clone(),
                    Some(monitor_idx),
                    run_loop.clone(),
                ) {
                    entry.insert(border)
                } else {
                    return Ok(());
                }
            }
        };

        let new_focus_state = if foreground_window == window.id {
            WindowKind::Floating
        } else {
            WindowKind::Unfocused
        };

        border.window_kind = new_focus_state;
        // Update the border's monitor idx in case it changed
        border.monitor_idx = Some(monitor_idx);

        border.update();

        windows_borders.insert(window.id, id);
    }

    Ok(())
}
