use crate::DISPLAY_INDEX_PREFERENCES;
use crate::Notification;
use crate::NotificationEvent;
use crate::WORKSPACE_MATCHING_RULES;
use crate::border_manager;
use crate::core::config_generation::WorkspaceMatchingRule;
use crate::macos_api::MacosApi;
use crate::monitor::Monitor;
use crate::notify_subscribers;
use crate::state::State;
use crate::window_manager::WindowManager;
use crossbeam_channel::Receiver;
use crossbeam_channel::RecvTimeoutError;
use crossbeam_channel::Sender;
use dispatch2::DispatchQueue;
use objc2_core_graphics::CGDirectDisplayID;
use parking_lot::Mutex;
use serde::Deserialize;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;
use strum::Display;

#[derive(Debug, Copy, Clone, Serialize, Deserialize, Display)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub enum MonitorNotification {
    Resize(CGDirectDisplayID),
    DisplayConnectionChange(CGDirectDisplayID),
}

static CHANNEL: OnceLock<(Sender<MonitorNotification>, Receiver<MonitorNotification>)> =
    OnceLock::new();

static MONITOR_CACHE: OnceLock<Mutex<HashMap<String, Monitor>>> = OnceLock::new();

pub fn channel() -> &'static (Sender<MonitorNotification>, Receiver<MonitorNotification>) {
    CHANNEL.get_or_init(|| crossbeam_channel::bounded(20))
}

fn event_tx() -> Sender<MonitorNotification> {
    channel().0.clone()
}

fn event_rx() -> Receiver<MonitorNotification> {
    channel().1.clone()
}

pub fn insert_in_monitor_cache(serial_number_id: &str, monitor: Monitor) {
    let display_index_preferences = DISPLAY_INDEX_PREFERENCES.read();
    let mut dip_ids = display_index_preferences.values();
    let preferred_id = if dip_ids.any(|id| id == serial_number_id) {
        monitor.serial_number_id.clone()
    } else {
        serial_number_id.to_string()
    };

    let mut monitor_cache = MONITOR_CACHE
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock();

    monitor_cache.insert(preferred_id, monitor);
}

pub fn send_notification(notification: MonitorNotification) {
    if event_tx().try_send(notification).is_err() {
        tracing::warn!("channel is full; dropping notification")
    }
}

/// How long the displays have to stay quiet before komorebi believes what they report.
///
/// Plugging or unplugging a display is never one event. macOS reports the old display
/// going, the new one arriving and the desktop changing shape, one callback each, and a TV
/// can drop and come back while it negotiates: logged on one afternoon, display 2 replaced
/// by 4 and 4 by 2 again two seconds later. The TV also does not always come back at its
/// real size -- a komorebi started right after a wake read it at 1920x1080 and kept that,
/// while it was 3008x1692 again by the next restart. Acting on each report laid the windows
/// out for a geometry that was gone a moment later, and read windows back while macOS was
/// still moving them about.
///
/// Three seconds of silence is longer than any gap seen inside one of those bursts, and
/// short enough not to leave the windows out of place for long.
const DISPLAY_SETTLE: Duration = Duration::from_secs(3);

/// How many times laying the windows out after a display change is tried before giving up.
///
/// Right after a wake, Accessibility does not answer for a while: windows report
/// `InvalidUIElement` even though nothing about them has changed. Measured: the TV came
/// back five seconds after the unlock, the two layouts that followed both failed that way,
/// and nothing tried again.
const LAYOUT_ATTEMPTS: usize = 3;

/// When macOS last said anything at all about the displays.
///
/// Set from the reconfiguration callback itself, on every call, including the ones that
/// announce a change before it happens and the ones that never reach this thread. That
/// covers the moment between the callback and the notification being picked up here.
static LAST_DISPLAY_EVENT: Mutex<Option<Instant>> = Mutex::new(None);

/// Whether a display change has been received and its windows not laid out again yet.
static RECONCILING: AtomicBool = AtomicBool::new(false);

/// Note that macOS has just reported something about the displays.
pub fn note_display_event() {
    *LAST_DISPLAY_EVENT.lock() = Some(Instant::now());
}

/// Whether the displays are changing, or have changed and the windows are not yet laid
/// out for them.
///
/// Until then, what the windows report says nothing about the windows. Their sizes are
/// the ones the old display gave them, or the ones macOS clamped them to while moving
/// them off it, and the work area komorebi lays them out in may still be the old screen's.
/// Two things went wrong because nothing held back in that window:
///
/// - Minimum sizes were learned from it. A window asked for a column of the new geometry
///   that still has the old one's width reports that width back, which reads exactly like
///   an application refusing to shrink: Ghostty "refused" 836 by staying at 1276, the
///   column it had a second earlier, in the pass that laid it out for a resolution change.
///   The minimum only ever grows: Notion, Mail and Ghostty once stood at 1484, half the
///   TV, and Terminal at 3005, all of it, until they were zeroed by hand.
/// - Windows were moved to other workspaces on the strength of it. BetterDisplay was sent
///   to workspace 2 by the very pass that laid it out for a resolution the user had just
///   switched to.
///
/// A display change is komorebi's to lay out, once, when it has settled; it is never a
/// reason to move a window to another workspace.
pub fn display_is_settling() -> bool {
    RECONCILING.load(Ordering::SeqCst)
        || LAST_DISPLAY_EVENT
            .lock()
            .is_some_and(|at| at.elapsed() < DISPLAY_SETTLE)
}

/// Holds [`RECONCILING`] up while one display change is handled, however that ends.
struct Reconciling;

impl Reconciling {
    fn begin() -> Self {
        RECONCILING.store(true, Ordering::SeqCst);
        Self
    }
}

impl Drop for Reconciling {
    fn drop(&mut self) {
        RECONCILING.store(false, Ordering::SeqCst);
    }
}

/// Wait out a burst of display events and return the one notification that stands for it.
///
/// See [`DISPLAY_SETTLE`]. A display arriving or leaving anywhere in the burst makes the
/// whole burst one: that path refreshes the size of every display too, and not the other
/// way round.
///
/// While the screen is covered -- asleep, locked, behind the screensaver -- Accessibility
/// cannot reach the windows, so laying them out would only fail. The wait carries on until
/// it lifts. Returns `None` once the channel has closed.
fn wait_for_displays_to_settle(
    receiver: &Receiver<MonitorNotification>,
    first: MonitorNotification,
) -> Option<MonitorNotification> {
    let mut settled = first;
    let mut events = 1;

    loop {
        match receiver.recv_timeout(DISPLAY_SETTLE) {
            Ok(next) => {
                events += 1;

                if matches!(next, MonitorNotification::DisplayConnectionChange(_))
                    || matches!(settled, MonitorNotification::Resize(_))
                {
                    settled = next;
                }
            }
            Err(RecvTimeoutError::Timeout) if crate::reaper::screen_is_covered() => {}
            Err(RecvTimeoutError::Timeout) => break,
            Err(RecvTimeoutError::Disconnected) => return None,
        }
    }

    // Grep marker: DISPLAY.
    tracing::warn!("DISPLAY settled after {events} report(s); handling it as {settled}");

    Some(settled)
}

/// Hand each monitor whose display has gone to a display that has taken its place.
///
/// This is the MacBook in clamshell mode: the TV and the built-in panel are never on at
/// the same time, so unplugging one and waking on the other takes one display away and
/// brings one in. The count stays at one, and "monitor counts match, reconciliation not
/// required" was the end of it. Komorebi went on laying windows out for a screen that was
/// no longer there -- the TV's 3004x1668 on the 1680-wide panel, windows hanging off the
/// edge and over each other; the panel's 1676 on the TV, everything crammed into the left
/// half, and the other workspaces parked at the panel's right edge, which on the TV is the
/// middle of the screen -- until `rset`.
/// Restarting on every wake used to hide this; the wake hook stopped restarting once
/// komorebi learned to keep its windows through a sleep.
///
/// The workspaces belong to the user, not to a display. The monitor keeps them, and
/// every window on them stays where it is; it takes on the identity and the geometry of
/// the display it is on now. Dropping the old monitor and starting a fresh one instead
/// would let go of every window and pick each one up again as if it had just opened, on
/// whichever workspace happened to be in front.
fn take_over_replaced_displays(wm: &mut WindowManager, attached_devices: &[Monitor]) {
    let mut newcomers = attached_devices
        .iter()
        .filter(|device| {
            !wm.monitors()
                .iter()
                .any(|monitor| monitor.serial_number_id == device.serial_number_id)
        })
        .collect::<Vec<_>>()
        .into_iter();

    for monitor in wm.monitors_mut() {
        if attached_devices
            .iter()
            .any(|device| device.serial_number_id == monitor.serial_number_id)
        {
            continue;
        }

        let Some(device) = newcomers.next() else {
            break;
        };

        tracing::warn!(
            "DISPLAY {} ({}) has been replaced by {} ({}, work area {}x{}); its workspaces stay as they are",
            monitor.device,
            monitor.serial_number_id,
            device.device,
            device.serial_number_id,
            device.work_area_size.right,
            device.work_area_size.bottom
        );

        monitor.id = device.id;
        monitor.device = device.device.clone();
        monitor.serial_number_id = device.serial_number_id.clone();
        monitor.size = device.size;
        monitor.work_area_size = device.work_area_size;
    }
}

/// Lay every monitor out again for the display it is on now.
///
/// The focused workspace goes into the new work area, and the windows of every other one
/// are parked again beside the new screen's edge: parked beside the old one, they can be
/// sitting in the middle of the new one. The rest are laid out as each one is shown, since
/// laying out a workspace means putting its windows on screen.
///
/// Nothing komorebi remembers about where windows are survives a display change: macOS
/// moves and shrinks windows itself when a display goes, and the reports of that can be
/// lost while the machine sleeps. Trusting those records here skipped windows that had
/// moved and parked others from a position they were no longer in.
///
/// Focus is left alone: the displays changing is not the user asking for anything.
fn lay_out_for_new_geometry(wm: &Arc<Mutex<WindowManager>>) {
    for attempt in 1..=LAYOUT_ATTEMPTS {
        while crate::reaper::screen_is_covered() {
            std::thread::sleep(DISPLAY_SETTLE);
        }

        let failed = {
            let mut wm = wm.lock();
            let mouse_follows_focus = wm.mouse_follows_focus;
            let mut failed = None;

            crate::window::forget_all_positions();

            for monitor in wm.monitors_mut() {
                tracing::warn!(
                    "DISPLAY laying out {} ({}) for a work area of {}x{}",
                    monitor.device,
                    monitor.serial_number_id,
                    monitor.work_area_size.right,
                    monitor.work_area_size.bottom
                );

                if let Err(error) =
                    monitor.load_focused_workspace_without_taking_focus(mouse_follows_focus)
                {
                    failed = Some(error);
                }
            }

            if let Err(error) = border_manager::destroy_all_borders() {
                tracing::error!("failed to take the borders down after a display change: {error}");
            }

            failed
        };

        let Some(error) = failed else {
            return;
        };

        tracing::warn!(
            "DISPLAY could not lay everything out (attempt {attempt} of {LAYOUT_ATTEMPTS}): {error}"
        );

        if attempt < LAYOUT_ATTEMPTS {
            std::thread::sleep(DISPLAY_SETTLE);
        }
    }
}

pub fn listen_for_notifications(wm: Arc<Mutex<WindowManager>>) -> color_eyre::Result<()> {
    std::thread::spawn(move || {
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

    Ok(())
}

#[tracing::instrument(skip_all)]
pub fn handle_notifications(wm: Arc<Mutex<WindowManager>>) -> color_eyre::Result<()> {
    tracing::info!("listening");
    let arc = wm.clone();

    let receiver = event_rx();

    'receiver: for first in receiver.iter() {
        // Held until this change has been laid out, however the handling below ends. See
        // [`display_is_settling`].
        let _reconciling = Reconciling::begin();

        let Some(notification) = wait_for_displays_to_settle(&receiver, first) else {
            break 'receiver;
        };

        let initial_state = {
            let wm = wm.lock();
            State::from(wm.as_ref())
        };

        // Whether the windows can have been left laid out for a geometry that is no longer
        // there. Adding or removing a monitor lays everything out on its own path below; a
        // display changing size, or taking another's place, is laid out here.
        let mut lay_out_again = false;

        match notification {
            MonitorNotification::Resize(_display_id) => {
                tracing::debug!("handling resize notification");
                DispatchQueue::main().exec_sync(|| {
                    let mut wm = wm.lock();
                    match MacosApi::update_monitor_work_areas(&mut wm) {
                        Ok(updated) => lay_out_again = updated,
                        Err(error) => {
                            tracing::error!("failed to update monitor work areas: {error}")
                        }
                    }
                });
            }
            MonitorNotification::DisplayConnectionChange(_) => 'connection: {
                tracing::debug!("handling display connection change notification");

                let mut attached_devices = vec![];
                DispatchQueue::main().exec_sync(|| {
                    if let Ok(latest) = MacosApi::latest_monitor_information() {
                        attached_devices = latest.clone();
                    }
                });

                let mut wm = wm.lock();
                let mut monitor_cache = MONITOR_CACHE
                    .get_or_init(|| Mutex::new(HashMap::new()))
                    .lock();

                let initial_monitor_count = wm.monitors().len();

                // Make sure that in our state any attached displays have the latest macOS API data
                for monitor in wm.monitors_mut() {
                    for attached in &attached_devices {
                        let serial_number_ids_match =
                            attached.serial_number_id == monitor.serial_number_id;

                        if serial_number_ids_match {
                            monitor.id = attached.id;
                            monitor.device = attached.device.clone();
                            monitor.serial_number_id = attached.serial_number_id.clone();
                            monitor.size = attached.size;
                            monitor.work_area_size = attached.work_area_size;
                        }
                    }
                }

                if initial_monitor_count == attached_devices.len() {
                    // Same number of displays, which is not the same as the same displays:
                    // see take_over_replaced_displays. Laid out again either way, because a
                    // display that came back can have come back at another size, and that
                    // part of the change was folded into this one while it settled.
                    take_over_replaced_displays(&mut wm, &attached_devices);
                    lay_out_again = true;
                    break 'connection;
                }

                if attached_devices.is_empty() {
                    tracing::debug!(
                        "no devices found, skipping reconciliation to avoid breaking state"
                    );
                    drop(wm);
                    continue 'receiver;
                }

                if initial_monitor_count > attached_devices.len() {
                    tracing::info!(
                        "monitor count mismatch ({initial_monitor_count} vs {}), removing disconnected monitors",
                        attached_devices.len()
                    );

                    // Windows to remove from `known_hwnds`
                    let mut windows_to_remove = Vec::new();

                    // Collect the ids in our state which aren't in the current attached display ids
                    // These are monitors that have been removed
                    let mut newly_removed_displays = vec![];

                    for (m_idx, m) in wm.monitors().iter().enumerate() {
                        if !attached_devices
                            .iter()
                            .any(|attached| attached.serial_number_id.eq(&m.serial_number_id))
                        {
                            let id = m.serial_number_id.clone();

                            newly_removed_displays.push(id.clone());

                            let focused_workspace_idx = m.focused_workspace_idx();

                            for (idx, workspace) in m.workspaces().iter().enumerate() {
                                let is_focused_workspace = idx == focused_workspace_idx;
                                let focused_container_idx = workspace.focused_container_idx();
                                for (c_idx, container) in workspace.containers().iter().enumerate()
                                {
                                    let focused_window_idx = container.focused_window_idx();
                                    for (w_idx, window) in container.windows().iter().enumerate() {
                                        windows_to_remove.push(window.id);
                                        if is_focused_workspace
                                            && c_idx == focused_container_idx
                                            && w_idx == focused_window_idx
                                        {
                                            // TODO: Do we need this on macOS?
                                            // Minimize the focused window since Windows might try
                                            // to move it to another monitor if it was focused.
                                            // if window.is_focused() {
                                            //     window.minimize()?;
                                            // }
                                        }
                                    }
                                }

                                // TODO: might not support maximization on macOS
                                // if let Some(maximized) = &workspace.maximized_window {
                                //     windows_to_remove.push(maximized.id);
                                //     // Minimize the focused window since Windows might try
                                //     // to move it to another monitor if it was focused.
                                //     if maximized.is_focused() {
                                //         maximized.minimize();
                                //     }
                                // }

                                if let Some(container) = &workspace.monocle_container {
                                    for window in container.windows() {
                                        windows_to_remove.push(window.id);
                                    }
                                    if let Some(_window) = container.focused_window() {
                                        // TODO: Do we need this on macOS?
                                        // Minimize the focused window since Windows might try
                                        // to move it to another monitor if it was focused.
                                        // if window.is_focused() {
                                        //     window.minimize();
                                        // }
                                    }
                                }

                                for window in workspace.floating_windows() {
                                    windows_to_remove.push(window.id);
                                    // TODO: Do we need this on macOS?
                                    // Minimize the focused window since Windows might try
                                    // to move it to another monitor if it was focused.
                                    // if window.is_focused() {
                                    //     window.minimize();
                                    // }
                                }
                            }

                            // Remove any workspace_rules for this specific monitor
                            let mut workspace_rules = WORKSPACE_MATCHING_RULES.lock();
                            let mut rules_to_remove = Vec::new();
                            for (i, rule) in workspace_rules.iter().enumerate().rev() {
                                if rule.monitor_index == m_idx {
                                    rules_to_remove.push(i);
                                }
                            }

                            for i in rules_to_remove {
                                workspace_rules.remove(i);
                            }

                            // Let's add their state to the cache for later, make sure to use what
                            // the user set as preference as the id.
                            let display_index_preferences = DISPLAY_INDEX_PREFERENCES.read();
                            let mut dip_ids = display_index_preferences.values();

                            let preferred_id = if dip_ids.any(|id| m.serial_number_id.eq(id)) {
                                m.serial_number_id.clone()
                            } else {
                                id.to_string()
                            };

                            monitor_cache.insert(preferred_id, m.clone());
                        }
                    }

                    // Update known_window_ids
                    wm.known_window_ids
                        .retain(|i, _| !windows_to_remove.contains(i));

                    if !newly_removed_displays.is_empty() {
                        // After we have cached them, remove them from our state
                        wm.monitors_mut().retain(|m| {
                            !newly_removed_displays
                                .iter()
                                .any(|id| m.serial_number_id.eq(id))
                        });
                    }

                    let post_removal_monitor_count = wm.monitors().len();
                    let focused_monitor_idx = wm.focused_monitor_idx();
                    if focused_monitor_idx >= post_removal_monitor_count {
                        wm.focus_monitor(0)?;
                    }

                    let offset = wm.work_area_offset;

                    for monitor in wm.monitors_mut() {
                        // If we have lost a monitor, update everything to filter out any jank
                        if initial_monitor_count != post_removal_monitor_count {
                            monitor.update_focused_workspace(offset)?;
                        }
                    }
                }

                let post_removal_monitor_count = wm.monitors().len();

                // This is the list of device ids after we have removed detached displays. We can
                // keep this with just the device_ids without the serial numbers since this is used
                // only to check which one is the newly added monitor below if there is a new
                // monitor. Everything done after with said new monitor will again consider both
                // serial number and device ids.
                let post_removal_serial_ids = wm
                    .monitors()
                    .iter()
                    .map(|m| &m.serial_number_id)
                    .cloned()
                    .collect::<Vec<_>>();

                // Check for and add any new monitors that may have been plugged in
                // Monitor and display index preferences get applied in this function
                {
                    // need to drop here because the main thread needs the lock to load the latest
                    // monitor info into the wm state
                    drop(wm);

                    DispatchQueue::main().exec_sync(|| {
                        let mut wm = arc.lock();
                        if let Err(error) = MacosApi::load_monitor_information(&mut wm) {
                            tracing::error!("failed to update load monitor information: {error}");
                        }
                    });
                }

                // regain the lock here once the monitor information has been updated
                let mut wm = arc.lock();

                let post_addition_monitor_count = wm.monitors().len();

                if post_addition_monitor_count > post_removal_monitor_count {
                    tracing::info!(
                        "monitor count mismatch ({post_removal_monitor_count} vs {post_addition_monitor_count}), adding connected monitors",
                    );

                    let known_window_ids = wm.known_window_ids.clone();
                    let offset = wm.work_area_offset;
                    let mouse_follows_focus = wm.mouse_follows_focus;
                    let focused_monitor_idx = wm.focused_monitor_idx();
                    let focused_workspace_idx = wm.focused_workspace_idx()?;

                    // Look in the updated state for new monitors
                    for (i, m) in wm.monitors_mut().iter_mut().enumerate() {
                        // let device_id = &m.device_id;
                        let serial_id = m.serial_number_id.clone();
                        // We identify a new monitor when we encounter a new device id
                        if !post_removal_serial_ids.contains(&serial_id) {
                            let mut cache_hit = false;
                            let mut cached_id = String::new();
                            // Check if that device id exists in the cache for this session
                            if let Some((id, cached)) = monitor_cache.get_key_value(&serial_id) {
                                cache_hit = true;
                                cached_id = id.clone();

                                tracing::info!(
                                    "found monitor and workspace configuration for {id} in the monitor cache, applying"
                                );

                                // If it does, update the cached monitor info with the new one and
                                // load the cached monitor removing any window that has since been
                                // closed or moved to another workspace
                                *m = Monitor {
                                    // Data that should be the one just read from `win32-display-data`
                                    id: m.id,
                                    // name: m.name.clone(),
                                    device: m.device.clone(),
                                    // device_id: m.device_id.clone(),
                                    serial_number_id: m.serial_number_id.clone(),
                                    size: m.size,
                                    work_area_size: m.work_area_size,

                                    // The rest should come from the cached monitor
                                    work_area_offset: cached.work_area_offset,
                                    window_based_work_area_offset: cached
                                        .window_based_work_area_offset,
                                    window_based_work_area_offset_limit: cached
                                        .window_based_work_area_offset_limit,
                                    workspaces: cached.workspaces.clone(),
                                    last_focused_workspace: cached.last_focused_workspace,
                                    // workspace_names: cached.workspace_names.clone(),
                                    container_padding: cached.container_padding,
                                    workspace_padding: cached.workspace_padding,
                                    wallpaper: cached.wallpaper.clone(),
                                    floating_layer_behaviour: cached.floating_layer_behaviour,
                                    window_hiding_position: cached.window_hiding_position,
                                };

                                let focused_workspace_idx = m.focused_workspace_idx();

                                for (j, workspace) in m.workspaces_mut().iter_mut().enumerate() {
                                    // If this is the focused workspace we need to show (restore) all
                                    // windows that were visible since they were probably minimized by
                                    // Windows.
                                    let is_focused_workspace = j == focused_workspace_idx;
                                    let focused_container_idx = workspace.focused_container_idx();

                                    let mut empty_containers = Vec::new();
                                    for (idx, container) in
                                        workspace.containers_mut().iter_mut().enumerate()
                                    {
                                        container.windows_mut().retain(|window| {
                                            window.exe().is_some()
                                                && !known_window_ids.contains_key(&window.id)
                                        });

                                        if container.windows().is_empty() {
                                            empty_containers.push(idx);
                                        }

                                        if is_focused_workspace {
                                            if let Some(_window) = container.focused_window() {
                                                // TODO: do we need to do this on macOS?
                                                // tracing::debug!(
                                                //     "restoring window: {}",
                                                //     window.id
                                                // );
                                                // WindowsApi::restore_window(window.hwnd);
                                            } else {
                                                // If the focused window was moved or removed by
                                                // the user after the disconnect then focus the
                                                // first window and show that one
                                                container.focus_window(0);
                                                //
                                                // if let Some(window) = container.focused_window() {
                                                //     WindowsApi::restore_window(window.hwnd);
                                                // }
                                            }
                                        }
                                    }

                                    // Remove empty containers
                                    for empty_idx in empty_containers {
                                        if empty_idx == focused_container_idx {
                                            workspace.remove_container(empty_idx);
                                        } else {
                                            workspace.remove_container_by_idx(empty_idx);
                                        }
                                    }

                                    // if let Some(window) = &workspace.maximized_window {
                                    //     if window.exe().is_none()
                                    //         || known_window_ids.contains_key(&window.id)
                                    //     {
                                    //         workspace.maximized_window = None;
                                    //     } else if is_focused_workspace {
                                    //         WindowsApi::restore_window(window.hwnd);
                                    //     }
                                    // }

                                    if let Some(container) = &mut workspace.monocle_container {
                                        container.windows_mut().retain(|window| {
                                            window.exe().is_some()
                                                && !known_window_ids.contains_key(&window.id)
                                        });

                                        if container.windows().is_empty() {
                                            workspace.monocle_container = None;
                                        } else if is_focused_workspace {
                                            if let Some(_window) = container.focused_window() {
                                                // WindowsApi::restore_window(window.hwnd);
                                            } else {
                                                // If the focused window was moved or removed by
                                                // the user after the disconnect then focus the
                                                // first window and show that one
                                                container.focus_window(0);

                                                if let Some(_window) = container.focused_window() {
                                                    // WindowsApi::restore_window(window.hwnd);
                                                }
                                            }
                                        }
                                    }

                                    workspace.floating_windows_mut().retain(|window| {
                                        window.exe().is_some()
                                            && !known_window_ids.contains_key(&window.id)
                                    });

                                    if is_focused_workspace {
                                        for _window in workspace.floating_windows() {
                                            // WindowsApi::restore_window(window.hwnd);
                                        }
                                    }

                                    // Apply workspace rules
                                    let mut workspace_matching_rules =
                                        WORKSPACE_MATCHING_RULES.lock();
                                    if let Some(rules) = workspace
                                        .workspace_config
                                        .as_ref()
                                        .and_then(|c| c.workspace_rules.as_ref())
                                    {
                                        for r in rules {
                                            workspace_matching_rules.push(WorkspaceMatchingRule {
                                                monitor_index: i,
                                                workspace_index: j,
                                                matching_rule: r.clone(),
                                                initial_only: false,
                                            });
                                        }
                                    }

                                    if let Some(rules) = workspace
                                        .workspace_config
                                        .as_ref()
                                        .and_then(|c| c.initial_workspace_rules.as_ref())
                                    {
                                        for r in rules {
                                            workspace_matching_rules.push(WorkspaceMatchingRule {
                                                monitor_index: i,
                                                workspace_index: j,
                                                matching_rule: r.clone(),
                                                initial_only: true,
                                            });
                                        }
                                    }
                                }

                                // Restore windows from new monitor and update the focused
                                // workspace
                                m.load_focused_workspace(mouse_follows_focus)?;
                                m.update_focused_workspace(offset)?;
                            }

                            // Entries in the cache should only be used once; remove the entry there was a cache hit
                            if cache_hit && !cached_id.is_empty() {
                                monitor_cache.remove(&cached_id);
                            }
                        }
                    }

                    // Refocus the previously focused monitor since the code above might
                    // steal the focus away.
                    wm.focus_monitor(focused_monitor_idx)?;
                    wm.focus_workspace(focused_workspace_idx)?;
                }

                let final_count = wm.monitors().len();

                if post_removal_monitor_count != final_count {
                    wm.retile_all(true)?;
                    // Second retile to fix DPI/resolution related jank
                    // wm.retile_all(true)?;
                    // Border updates to fix DPI/resolution related jank
                    // border_manager::send_notification(None);
                }

                let mouse_follows_focus = wm.mouse_follows_focus;
                // when a monitor is added/removed some of our "hidden" windows
                // get brought back onto the screen, so we wanna make sure unfocused
                // workspaces get hidden again
                if initial_monitor_count != final_count {
                    for monitor in wm.monitors_mut() {
                        monitor.load_focused_workspace(mouse_follows_focus)?;
                    }
                }
            }
        }

        if lay_out_again {
            lay_out_for_new_geometry(&wm);
        }

        {
            let wm = wm.lock();
            notify_subscribers(
                Notification {
                    event: NotificationEvent::Monitor(notification),
                    state: wm.as_ref().into(),
                },
                initial_state.has_been_modified(&wm),
            )?;
        }

        border_manager::send_notification(None, None, false);
    }

    Ok(())
}
