use std::sync::LazyLock;
use crate::AccessibilityObserver;
use crate::AccessibilityUiElement;
use crate::FLOATING_APPLICATIONS;
use crate::FLOATING_WINDOW_TOGGLE_ASPECT_RATIO;
use crate::IGNORE_IDENTIFIERS;
use crate::LibraryError;
use crate::MANAGE_IDENTIFIERS;
use crate::PERMAIGNORE_CLASSES;
use crate::REGEX_IDENTIFIERS;
use crate::TABBED_APPLICATIONS;
use crate::TITLELESS_APPLICATIONS;
use crate::WINDOW_RESTORE_POSITIONS;
use crate::accessibility::AccessibilityApi;
use crate::accessibility::action_constants::kAXPressAction;
use crate::accessibility::attribute_constants::kAXCloseButtonAttribute;
use crate::accessibility::attribute_constants::kAXFocusedAttribute;
use crate::accessibility::attribute_constants::kAXMainAttribute;
use crate::accessibility::attribute_constants::kAXMinimizedAttribute;
use crate::accessibility::attribute_constants::kAXParentAttribute;
use crate::accessibility::attribute_constants::kAXPositionAttribute;
use crate::accessibility::attribute_constants::kAXRoleAttribute;
use crate::accessibility::attribute_constants::kAXSizeAttribute;
use crate::accessibility::attribute_constants::kAXSubroleAttribute;
use crate::accessibility::attribute_constants::kAXTitleAttribute;
use crate::accessibility::error::AccessibilityCustomError;
use crate::accessibility::error::AccessibilityError;
use crate::accessibility::notification_constants::AccessibilityNotification;
use crate::accessibility::notification_constants::kAXTitleChangedNotification;
use crate::accessibility::notification_constants::kAXWindowDeminiaturizedNotification;
use crate::accessibility::notification_constants::kAXWindowMiniaturizedNotification;
use crate::accessibility::notification_constants::kAXWindowMovedNotification;
use crate::accessibility::notification_constants::kAXWindowResizedNotification;
use crate::animation::ANIMATION_DURATION_GLOBAL;
use crate::animation::ANIMATION_DURATION_PER_ANIMATION;
use crate::animation::ANIMATION_ENABLED_GLOBAL;
use crate::animation::ANIMATION_ENABLED_PER_ANIMATION;

use crate::accessibility::private::EnhancedUiHeldOff;
use crate::accessibility::private::with_enhanced_ui_disabled;
use crate::animation::ANIMATION_STYLE_GLOBAL;
use crate::animation::ANIMATION_STYLE_PER_ANIMATION;
use crate::animation::AnimationEngine;
use crate::animation::RenderDispatcher;
use crate::animation::lerp::Lerp;
use crate::animation::prefix::AnimationPrefix;
use crate::animation::prefix::new_animation_key;
use crate::application::Application;
use crate::cf_dictionary_value;
use crate::core::ApplicationIdentifier;
use crate::core::Rect;
use crate::core::WindowHidingPosition;
use crate::core::animation::AnimationStyle;
use crate::core::config_generation::IdWithIdentifier;
use crate::core::config_generation::MatchingRule;
use crate::core::config_generation::MatchingStrategy;
use crate::core_graphics::CoreGraphicsApi;
use crate::hidden_frame_bottom_left;
use crate::hidden_frame_bottom_right;
use crate::macos_api::MacosApi;
use crate::reaper;
use crate::reaper::ReaperNotification;
use crate::window_manager_event::SystemNotification;
use crate::window_manager_event::WindowManagerEvent;
use crate::window_manager_event_listener;
use color_eyre::eyre;
use objc2::__framework_prelude::Retained;
use objc2_app_kit::NSApplicationActivationOptions;
use objc2_app_kit::NSRunningApplication;
use objc2_application_services::AXObserver;
use objc2_application_services::AXUIElement;
use objc2_application_services::AXValueType;
use objc2_core_foundation::CFBoolean;
use objc2_core_foundation::CFDictionary;
use objc2_core_foundation::CFNumber;
use objc2_core_foundation::CFRetained;
use objc2_core_foundation::CFRunLoop;
use objc2_core_foundation::CFString;
use objc2_core_foundation::CGFloat;
use objc2_core_foundation::CGPoint;
use objc2_core_foundation::CGSize;
use objc2_core_graphics::kCGWindowAlpha;
use objc2_core_graphics::kCGWindowBounds;
use objc2_core_graphics::kCGWindowName;
use objc2_core_graphics::kCGWindowNumber;
use objc2_core_graphics::kCGWindowOwnerName;
use objc2_core_graphics::kCGWindowOwnerPID;
use objc2_foundation::NSBundle;
use objc2_foundation::NSString;
use regex::Regex;
use serde::Deserialize;
use serde::Serialize;
use serde::Serializer;
use serde::ser::SerializeStruct;
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::ffi::c_void;
use std::fmt::Display;
use std::fmt::Formatter;
use std::fmt::Write;
use std::path::Path;
use std::path::PathBuf;
use std::ptr::NonNull;
use std::str::FromStr;
use std::sync::atomic::Ordering;
use std::time::Duration;

use strum::Display;
use strum::EnumString;
use tracing::instrument;

const NOTIFICATIONS: &[&str] = &[
    kAXWindowMiniaturizedNotification,
    kAXWindowDeminiaturizedNotification,
    kAXWindowMovedNotification,
    kAXWindowResizedNotification,
    kAXTitleChangedNotification,
];

lazy_static::lazy_static! {
    /// Where komorebi last confirmed each window to be.
    ///
    /// Checking whether a window is already in place used to ask the application every
    /// time -- a synchronous round trip per window, per layout pass, for something
    /// komorebi itself decided. It put the window there and watched it land; there is no
    /// need to ask again.
    ///
    /// Invalidated the moment a window moves for any other reason (see forget_position),
    /// so a window the user drags is never assumed to be where it was left.
    static ref CONFIRMED_POSITIONS: parking_lot::Mutex<HashMap<u32, Rect>> =
        parking_lot::Mutex::new(HashMap::new());
}

/// Which application owns a window, asked of the window server.
///
/// For windows komorebi knows nothing about -- which is the only time it needs asking.
pub fn window_owner_name(window_id: u32) -> Option<String> {
    let list = crate::core_graphics::CoreGraphicsApi::window_list_info()?;

    crate::cf_array_as::<objc2_core_foundation::CFDictionary>(&list)
        .into_iter()
        .map(WindowInfo::new)
        .find(|info| info.window_id == Some(window_id))
        .map(|info| info.owner_name)
}

/// Windows already reported by the MANAGING trace, so it speaks once per window rather
/// than on every event that window produces.
static MANAGE_LOGGED: LazyLock<parking_lot::Mutex<std::collections::HashSet<u32>>> =
    LazyLock::new(|| parking_lot::Mutex::new(std::collections::HashSet::new()));

/// Window titles, asked for once and remembered.
///
/// Saving the session names every window by application and title so the layout can be
/// rebuilt after a login, and that happens after every command -- so every command was
/// asking every window for its title, one call into its process each.
///
/// Unlike an application's name, a title genuinely changes: a terminal follows the
/// directory, a browser follows the tab. macOS says when, and komorebi is already
/// listening for it on every window, so the entry is dropped then and read again once.
static WINDOW_TITLES: LazyLock<parking_lot::Mutex<HashMap<u32, Option<String>>>> =
    LazyLock::new(|| parking_lot::Mutex::new(HashMap::new()));

/// The title this window has changed, or it has gone: ask again next time.
pub fn forget_title(window_id: u32) {
    WINDOW_TITLES.lock().remove(&window_id);
}

/// Move and resize events komorebi's own placements are about to cause.
///
/// Moving a window makes macOS report that the window moved, and that report arrives
/// indistinguishable from the window having moved on its own. The position cache was
/// therefore erased by the very placement that had just filled it, which is why it never
/// once produced a hit: every layout pass went back to asking each application where its
/// window was.
///
/// So komorebi says in advance how many reports its own move is about to generate, and
/// each of those is absorbed rather than treated as news. The count is *set* on every
/// placement, never added to, so a report that never arrives -- a dropped event, an
/// application that does not send one -- cannot accumulate and silently swallow a real
/// move later on.
static SELF_MOVE_ECHOES: LazyLock<parking_lot::Mutex<HashMap<u32, u8>>> =
    LazyLock::new(|| parking_lot::Mutex::new(HashMap::new()));

fn expect_self_move_echoes(window_id: u32, count: u8) {
    SELF_MOVE_ECHOES.lock().insert(window_id, count);
}

/// Whether this report is one of komorebi's own, and should not be believed as news.
pub fn absorb_self_move_echo(window_id: u32) -> bool {
    let mut echoes = SELF_MOVE_ECHOES.lock();

    match echoes.get_mut(&window_id) {
        Some(remaining) if *remaining > 0 => {
            *remaining -= 1;
            true
        }
        _ => false,
    }
}

/// Forget where a window was: it has moved for reasons of its own.
pub fn forget_position(window_id: u32) {
    CONFIRMED_POSITIONS.lock().remove(&window_id);
    SELF_MOVE_ECHOES.lock().remove(&window_id);
}

/// A window that has gone: nothing remembered about it is worth keeping, and the next
/// window to be handed this id is a different one.
pub fn forget_window(window_id: u32) {
    forget_position(window_id);
    forget_title(window_id);
}

/// TIMING: reports how long hiding one window took, however it returns.
struct TimedHide {
    started: std::time::Instant,
    window_id: u32,
    application: Option<String>,
}

/// How long the geometry read at the top of `hide` took, per window.
static HIDE_READ: LazyLock<parking_lot::Mutex<HashMap<u32, std::time::Duration>>> =
    LazyLock::new(|| parking_lot::Mutex::new(HashMap::new()));

impl Drop for TimedHide {
    fn drop(&mut self) {
        let elapsed = self.started.elapsed();
        if elapsed.as_millis() >= 2 {
            tracing::warn!(
                "TIMING hide window={} app={:?} took={}ms read={}ms",
                self.window_id,
                self.application.clone().unwrap_or_default(),
                elapsed.as_millis(),
                HIDE_READ
                    .lock()
                    .remove(&self.window_id)
                    .unwrap_or_default()
                    .as_millis()
            );
        }
    }
}

/// Render dispatcher for window movement animations
pub struct MovementRenderDispatcher {
    window_id: u32,
    element: AccessibilityUiElement,
    observer: AccessibilityObserver,
    start_rect: Rect,
    target_rect: Rect,
    style: AnimationStyle,
}

impl MovementRenderDispatcher {
    pub const PREFIX: AnimationPrefix = AnimationPrefix::Movement;

    pub fn new(
        window_id: u32,
        element: AccessibilityUiElement,
        observer: AccessibilityObserver,
        start_rect: Rect,
        target_rect: Rect,
        style: AnimationStyle,
    ) -> Self {
        Self {
            window_id,
            element,
            observer,
            start_rect,
            target_rect,
            style,
        }
    }
}

impl RenderDispatcher for MovementRenderDispatcher {
    fn get_animation_key(&self) -> String {
        new_animation_key(MovementRenderDispatcher::PREFIX, self.window_id.to_string())
    }

    fn pre_render(&self) -> eyre::Result<()> {
        // Remove move/resize notifications during animation to prevent
        // flooding the event channel with notifications we generated ourselves
        if let Some(observer) = &self.observer.0 {
            let _ = AccessibilityApi::remove_notification_from_observer(
                observer,
                &self.element,
                kAXWindowMovedNotification,
            );
            let _ = AccessibilityApi::remove_notification_from_observer(
                observer,
                &self.element,
                kAXWindowResizedNotification,
            );
        }
        Ok(())
    }

    fn render(&self, progress: f64) -> eyre::Result<()> {
        let new_rect = self.start_rect.lerp(self.target_rect, progress, self.style);

        with_enhanced_ui_disabled(&self.element, || {
            let _ = AccessibilityApi::set_attribute_ax_value(
                &self.element,
                kAXPositionAttribute,
                AXValueType::CGPoint,
                CGPoint::new(new_rect.left as CGFloat, new_rect.top as CGFloat),
            );

            let _ = AccessibilityApi::set_attribute_ax_value(
                &self.element,
                kAXSizeAttribute,
                AXValueType::CGSize,
                CGSize::new(new_rect.right as CGFloat, new_rect.bottom as CGFloat),
            );
        });

        Ok(())
    }

    fn post_render(&self) -> eyre::Result<()> {
        // Exact final position via AX so the app syncs its internal state
        with_enhanced_ui_disabled(&self.element, || {
            let _ = AccessibilityApi::set_attribute_ax_value(
                &self.element,
                kAXPositionAttribute,
                AXValueType::CGPoint,
                CGPoint::new(
                    self.target_rect.left as CGFloat,
                    self.target_rect.top as CGFloat,
                ),
            );

            let _ = AccessibilityApi::set_attribute_ax_value(
                &self.element,
                kAXSizeAttribute,
                AXValueType::CGSize,
                CGSize::new(
                    self.target_rect.right as CGFloat,
                    self.target_rect.bottom as CGFloat,
                ),
            );
        });

        // Restore move/resize notifications
        if let Some(observer) = &self.observer.0 {
            let _ = AccessibilityApi::add_notification_to_observer(
                observer,
                &self.element,
                kAXWindowMovedNotification,
                None,
            );
            let _ = AccessibilityApi::add_notification_to_observer(
                observer,
                &self.element,
                kAXWindowResizedNotification,
                None,
            );
        }

        Ok(())
    }
}

#[instrument(skip_all)]
unsafe extern "C-unwind" fn window_observer_callback(
    _observer: NonNull<AXObserver>,
    element: NonNull<AXUIElement>,
    notification: NonNull<CFString>,
    _context: *mut c_void,
) {
    unsafe {
        // DIAGNOSTIC: everything the system sends us at the window level, before any
        // filtering. Grep marker: RAWWIN.
        {
            let mut pid = 0;
            element.as_ref().pid(NonNull::from_mut(&mut pid));
            tracing::info!(
                "RAWWIN {} pid={pid}",
                notification.as_ref().to_string()
            );
        }

        let name =
            AccessibilityApi::copy_attribute_value::<CFString>(element.as_ref(), kAXTitleAttribute)
                .map(|s| s.to_string());

        if let Some(name) = name
            && !name.is_empty()
        {
            let mut process_id = 0;
            element.as_ref().pid(NonNull::from_mut(&mut process_id));

            let window_id = AccessibilityApi::window_id(element.as_ref()).ok();

            // The one thing that makes a remembered title wrong. Dropped here rather than
            // further in, because this callback sees every window notification whether or
            // not it turns into something komorebi acts on.
            if notification.as_ref().to_string() == kAXTitleChangedNotification
                && let Some(window_id) = window_id
            {
                forget_title(window_id);
            }

            if let Ok(notification) =
                AccessibilityNotification::from_str(&notification.as_ref().to_string())
                && let Some(event) = WindowManagerEvent::from_system_notification(
                    SystemNotification::Accessibility(notification),
                    process_id,
                    window_id,
                )
            {
                tracing::debug!(
                    "notification: {}, process: {process_id}, name: \"{name}\"",
                    notification,
                );

                window_manager_event_listener::send_notification(event);
            }
        }
    }
}

#[derive(Debug, Default)]
pub struct WindowInfo {
    name: Option<String>,
    pub owner_pid: i32,
    owner_name: String,
    alpha: f32,
    bounds: WindowBounds,
    window_id: Option<u32>,
}

impl WindowInfo {
    pub fn new(entry: NonNull<CFDictionary>) -> Self {
        WindowInfo::from(unsafe { entry.as_ref() })
    }
}

#[derive(Debug, Default)]
#[allow(unused)]
pub struct ValidWindowInfo {
    pub name: String,
    pub owner_pid: i32,
    owner_name: String,
    alpha: f32,
    pub bounds: WindowBounds,
    pub window_id: u32,
}

impl WindowInfo {
    pub fn validated(self) -> Option<ValidWindowInfo> {
        if let Some(name) = self.name
            && let Some(window_id) = self.window_id
            && self.alpha != 0.0
            && self.bounds.y != 0.0
            && self.bounds.height != 0.0
            && !name.is_empty()
        {
            return Some(ValidWindowInfo {
                name,
                owner_pid: self.owner_pid,
                owner_name: self.owner_name,
                alpha: self.alpha,
                bounds: self.bounds,
                window_id,
            });
        }

        None
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Window {
    pub id: u32,
    #[serde(skip_deserializing)]
    pub element: AccessibilityUiElement,
    #[serde(skip_deserializing)]
    pub application: Application,
    #[serde(skip_deserializing)]
    observer: AccessibilityObserver,
    pub details: Option<WindowDetails>,
}

#[cfg(test)]
impl From<u32> for Window {
    fn from(id: u32) -> Self {
        Self {
            id,
            element: Default::default(),
            application: Default::default(),
            observer: Default::default(),
            details: None,
        }
    }
}

#[allow(clippy::module_name_repetitions)]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WindowDetails {
    pub title: String,
    pub exe: String,
    pub role: String,
    pub subrole: String,
    pub icon_path: PathBuf,
}

impl From<&Window> for WindowDetails {
    fn from(value: &Window) -> Self {
        Self {
            title: value.title().unwrap_or_default(),
            exe: value.exe().unwrap_or_default(),
            role: value.role().unwrap_or_default(),
            subrole: value.subrole().unwrap_or_default(),
            icon_path: value.icon_path().unwrap_or_default(),
        }
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        // this gets called when a cloned Window is dropped, so we need to make sure it only
        // invalidates the observer if the Window is no longer open
        if !self.is_valid() {
            tracing::info!(
                "invalidating window observer for {}",
                self.title()
                    .unwrap_or_else(|| String::from("<NO TITLE FOUND>"))
            );

            // Messages wouldn't close on click which is annoying, this handles that edge case
            // for now - hopefully it doesn't break anything else
            reaper::send_notification(ReaperNotification::InvalidWindow(self.id));

            // make sure the observer gets removed from any run loops
            AccessibilityApi::invalidate_observer(&self.observer);
        }
    }
}

impl Display for Window {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let mut display = format!(
            "(window_id: {}, process_id: {}",
            self.id, self.application.process_id
        );

        if let Some(title) = self.title() {
            write!(display, ", title: {title}")?;
        }

        if let Some(exe) = self.exe() {
            write!(display, ", exe: {exe}")?;
        }

        if let Some(role) = self.role() {
            write!(display, ", role: {role}")?;
        }

        if let Some(subrole) = self.subrole() {
            write!(display, ", subrole: {subrole}")?;
        }

        write!(display, ")")?;

        write!(f, "{display}")
    }
}

impl Serialize for Window {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut state = serializer.serialize_struct("Window", 6)?;
        state.serialize_field("id", &self.id)?;
        state.serialize_field(
            "rect",
            &Rect::from(MacosApi::window_rect(&self.element).unwrap_or_default()),
        )?;
        state.serialize_field("details", &WindowDetails::from(self))?;
        state.end()
    }
}

#[cfg(feature = "schemars")]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
struct SerializedWindow {
    id: u32,
    title: String,
    exe: String,
    role: String,
    subrole: String,
    rect: Rect,
}

#[cfg(feature = "schemars")]
impl schemars::JsonSchema for Window {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed("Window")
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        <SerializedWindow as schemars::JsonSchema>::json_schema(generator)
    }
}

impl Window {
    pub fn new(
        element: CFRetained<AXUIElement>,
        application: Application,
    ) -> Result<Self, AccessibilityError> {
        let observer = AccessibilityApi::create_observer(
            application.process_id,
            Some(window_observer_callback),
        )?;

        Ok(Self {
            id: AccessibilityApi::window_id(&element)?,
            element: AccessibilityUiElement(element),
            application,
            observer: AccessibilityObserver(Some(observer)),
            details: None,
        })
    }

    pub fn is_valid(&self) -> bool {
        AccessibilityApi::copy_attribute_names(&self.element).is_some()
    }

    pub fn is_focused(&self) -> bool {
        MacosApi::foreground_window_id().unwrap_or_default() == self.id
    }

    #[tracing::instrument(skip_all)]
    pub fn observe(
        &self,
        run_loop: &CFRunLoop,
        refcon: Option<*mut c_void>,
    ) -> Result<(), AccessibilityError> {
        tracing::info!("registering observer for {self}");

        AccessibilityApi::add_observer_to_run_loop(
            &self.observer,
            &self.element,
            NOTIFICATIONS,
            run_loop,
            refcon,
        )
    }

    #[tracing::instrument(skip_all)]
    pub fn hide(
        &mut self,
        hiding_position: WindowHidingPosition,
    ) -> Result<(), AccessibilityError> {
        let started = std::time::Instant::now();
        let _timing = TimedHide {
            started,
            window_id: self.id,
            application: self.application.name(),
        };

        // Hiding needs to know where the window is, to note the restore point and to work
        // out which display it is on. It used to ask the application, and that turned out
        // to be the whole cost of hiding: 144ms of WhatsApp's 144ms, 42 of Mail's 43.
        // Parking the window afterwards is nearly free by comparison.
        //
        // The answer is already known. Komorebi put the window where it is and remembers
        // doing so, and that memory is dropped the moment anything else moves it. Asking
        // was asking a question it had written down.
        let reading_started = std::time::Instant::now();

        let rect = match CONFIRMED_POSITIONS.lock().get(&self.id).copied() {
            Some(known) => objc2_core_foundation::CGRect::from(known),
            None => MacosApi::window_rect(&self.element)?,
        };

        HIDE_READ.lock().insert(self.id, reading_started.elapsed());

        let mut window_restore_positions = WINDOW_RESTORE_POSITIONS.lock();
        if let Entry::Vacant(entry) = window_restore_positions.entry(self.id) {
            entry.insert(rect);
            drop(window_restore_positions);
        }

        if let Some(monitor_size) = CoreGraphicsApi::display_bounds_for_window_rect(rect) {
            // I don't love this, but it's basically what Aerospace does in lieu of an actual "Hide" API
            let hidden_rect = match hiding_position {
                WindowHidingPosition::BottomLeft => {
                    hidden_frame_bottom_left(monitor_size, rect.size)
                }
                WindowHidingPosition::BottomRight => {
                    hidden_frame_bottom_right(monitor_size, rect.size)
                }
            };

            tracing::debug!(
                "hiding {} and setting restore point to {},{}",
                self.title()
                    .unwrap_or_else(|| String::from("<NO TITLE FOUND>")),
                rect.origin.x,
                rect.origin.y,
            );

            // EUI disabled so hiding is instant and the window doesn't animate
            // as it moves off-screen (same reason as in set_position_direct).
            // Hiding parks the window off-screen at the size it already has, so the
            // resize half of this is a request to stay exactly as it is -- and a resize
            // is not a cheap no-op: the application relayouts its whole interface before
            // answering. WhatsApp charges up to 240ms for one. Compare first and only
            // ask for what actually differs.
            // Already parked exactly here? Then there is nothing to do.
            //
            // Every workspace change parks every window of every workspace being left,
            // whether or not it was already parked, so most of that work is repeated for
            // windows that have not moved since the last time. With five workspaces open
            // it is most of the parking done.
            //
            // The comparison is against what komorebi recorded when it put the window
            // there, not against a guess at whether it is hidden. That record is dropped
            // the moment anything else moves the window, so a window that came back for
            // any reason does not match and gets parked properly.
            if CONFIRMED_POSITIONS
                .lock()
                .get(&self.id)
                .is_some_and(|known| *known == Rect::from(hidden_rect))
            {
                return Ok(());
            }

            let resizing = hidden_rect.size.width != rect.size.width
                || hidden_rect.size.height != rect.size.height;

            expect_self_move_echoes(self.id, if resizing { 2 } else { 1 });

            let _enhanced_ui = self.hold_enhanced_ui_off();

            self.set_point(hidden_rect.origin, true)?;

            if resizing {
                self.set_size(hidden_rect.size, true)?;
            }

            // Parked off-screen is still a place komorebi knows the window to be, and
            // knowing it is what lets the move back on screen happen without asking.
            CONFIRMED_POSITIONS
                .lock()
                .insert(self.id, Rect::from(hidden_rect));
        }

        Ok(())
    }

    pub fn minimize(&mut self) -> Result<(), AccessibilityError> {
        let cf_boolean = CFBoolean::new(true);
        let value = &**cf_boolean;
        AccessibilityApi::set_attribute_cf_value(&self.element, kAXMinimizedAttribute, value)
    }

    pub fn unminimize(&mut self) -> Result<(), AccessibilityError> {
        let cf_boolean = CFBoolean::new(false);
        let value = &**cf_boolean;
        AccessibilityApi::set_attribute_cf_value(&self.element, kAXMinimizedAttribute, value)
    }

    #[tracing::instrument(skip_all)]
    pub fn restore(&mut self) -> Result<(), AccessibilityError> {
        let started = std::time::Instant::now();
        let mut should_remove_restore_position = false;
        let mut window_restore_positions = WINDOW_RESTORE_POSITIONS.lock();
        if let Some(cg_rect) = window_restore_positions.get(&self.id) {

            tracing::debug!(
                "restoring {:?} to {cg_rect:?}",
                self.title()
                    .unwrap_or_else(|| String::from("<NO TITLE FOUND>"))
            );

            // Hiding never changed the size, so the size it has now is almost always the
            // size being restored. Asking for it anyway makes the application relayout
            // for nothing. One read says whether either half is needed at all.
            let current = MacosApi::window_rect(&self.element).ok();

            let point_differs = current.is_none_or(|current| {
                current.origin.x != cg_rect.origin.x || current.origin.y != cg_rect.origin.y
            });

            let size_differs = current.is_none_or(|current| {
                current.size.width != cg_rect.size.width
                    || current.size.height != cg_rect.size.height
            });

            if point_differs {
                self.set_point(cg_rect.origin, true)?;
            }

            if size_differs {
                self.set_size(cg_rect.size, true)?;
            }

            // TIMING: the other half of a workspace change, and until now unmeasured.
            let elapsed = started.elapsed();
            if elapsed.as_millis() >= 2 {
                tracing::warn!(
                    "TIMING restore window={} app={:?} took={}ms point={} size={}",
                    self.id,
                    self.application.name().unwrap_or_default(),
                    elapsed.as_millis(),
                    point_differs,
                    size_differs
                );
            }

            should_remove_restore_position = true;
        }

        if should_remove_restore_position {
            window_restore_positions.remove(&self.id);
        }

        Ok(())
    }

    pub fn title(&self) -> Option<String> {
        if let Some(known) = WINDOW_TITLES.lock().get(&self.id) {
            return known.clone();
        }

        let title = AccessibilityApi::copy_attribute_value::<CFString>(
            &self.element,
            kAXTitleAttribute,
        )
        .map(|s| s.to_string());

        // Only a real title is worth remembering. A window that has no title yet is not
        // a window with no title: it is one that has not finished opening, and the answer
        // changes within milliseconds. Remembering the empty answer freezes it -- and
        // whether komorebi manages a window at all depends on it having a title, so a
        // window caught at that moment would be ignored for as long as it stayed open.
        if title.as_deref().is_some_and(|title| !title.is_empty()) {
            WINDOW_TITLES.lock().insert(self.id, title.clone());
        }

        title
    }

    pub fn exe(&self) -> Option<String> {
        self.application.name()
    }

    pub fn bundle_identifier(&self) -> Option<String> {
        if let Ok(Some(identifier)) = self.running_application().map(|app| app.bundleIdentifier()) {
            Some(identifier.to_string())
        } else {
            None
        }
    }

    pub fn bundle_path(&self) -> Option<PathBuf> {
        if let Ok(Some(path)) = self
            .running_application()
            .map(|app| app.bundleURL())
            .map(|url| url.map(|url| url.to_file_path()))
        {
            path
        } else {
            None
        }
    }

    pub fn path(&self) -> Option<PathBuf> {
        if let Ok(Some(path)) = self
            .running_application()
            .map(|app| app.executableURL())
            .map(|ns_url| ns_url.map(|url| url.to_file_path()))
        {
            path
        } else {
            None
        }
    }

    pub fn icon_path(&self) -> Option<PathBuf> {
        if let Some(path) = self.bundle_path()
            && let Some(bundle) =
                NSBundle::bundleWithPath(&NSString::from_str(&path.to_string_lossy()))
            && let Some(icon_file) =
                bundle.objectForInfoDictionaryKey(&NSString::from_str("CFBundleIconFile"))
            && let Ok(icon_name) = icon_file.downcast::<NSString>()
        {
            let mut icon_path = format!("{}/Contents/Resources/{}", path.display(), icon_name);

            if !icon_path.ends_with(".icns") {
                icon_path.push_str(".icns");
            }

            let path = Path::new(&icon_path);

            if path.exists() {
                return Some(PathBuf::from(path));
            }
        }

        None
    }

    pub fn role(&self) -> Option<String> {
        AccessibilityApi::copy_attribute_value::<CFString>(&self.element, kAXRoleAttribute)
            .map(|s| s.to_string())
    }

    pub fn subrole(&self) -> Option<String> {
        AccessibilityApi::copy_attribute_value::<CFString>(&self.element, kAXSubroleAttribute)
            .map(|s| s.to_string())
    }

    fn running_application(&self) -> Result<Retained<NSRunningApplication>, AccessibilityError> {
        NSRunningApplication::runningApplicationWithProcessIdentifier(self.application.process_id)
            .ok_or(AccessibilityError::Custom(
                AccessibilityCustomError::NSRunningApplication(self.application.process_id),
            ))
    }

    pub fn set_position(&self, rect: &Rect) -> Result<(), AccessibilityError> {
        // Ask for a size the application will actually accept.
        //
        // An application that refuses a width does not refuse it cheaply: WhatsApp takes
        // up to 220ms to decline, measured, and it declines every single time because the
        // request never changes. Worse, refusing means the window never ends up where it
        // was put, so the "already in place" check below never matches it and every
        // layout pass pays that cost again.
        //
        // Its minimum is already known -- learned once and remembered on disk -- so
        // asking for that instead makes the request one it can satisfy. The window ends
        // up exactly where it would have anyway; the difference is that komorebi stops
        // arguing about it, and from the next pass on skips it entirely.
        let mut effective = *rect;

        if let Some(application) = self.application.name() {
            if let Some(minimum) = crate::min_size::get(&application)
                && minimum > effective.right
            {
                effective.right = minimum;
            }

            if let Some(minimum) = crate::min_size::get_height(&application)
                && minimum > effective.bottom
            {
                effective.bottom = minimum;
            }
        }

        let rect = &effective;

        // Moving a window makes macOS emit AXWindowMoved and AXWindowResized, which
        // come straight back to us as events, and handling those can ask for another
        // layout pass. Callers position every window of a workspace unconditionally,
        // so a window already sitting where it belongs was still being told to move
        // there again -- measured at 938 identical requests to the same two windows
        // in 164 seconds, each one manufacturing two events for the queue to carry.
        //
        // If it is already in place, do nothing. A window that refuses the geometry (see
        // the mismatch warning below) never matches and keeps being retried -- that case
        // is what the per-application minimum size handling is for, not this guard.
        //
        // The second question is whether the window is already the right size and only in
        // the wrong place. That is
        // not a rare case, it is the common one: hiding a window parks it off-screen
        // without touching its size, so everything coming back from a hidden workspace
        // needs a move and nothing else. Asking for the size anyway makes the application
        // relayout its whole interface for a value it already has -- measured at over
        // 100ms per pass for WhatsApp, and it happens on every workspace change.
        let mut size_already_correct = false;
        let mut had_size = (0, 0);

        // TIMING: the cost of asking the application where its window is, before moving
        // it. This is the read a working position cache would remove -- every placement
        // pays it, hit or miss -- and until now it was the one call on this path that was
        // never measured, because the stopwatch below starts after it.
        let asking_started = std::time::Instant::now();

        // Where komorebi last put this window, if it still knows. Only when it does not
        // is the application asked, which after the first placement is almost never.
        let known = CONFIRMED_POSITIONS.lock().get(&self.id).copied();

        let current_position = match known {
            Some(known) => Some(known),
            None => MacosApi::window_rect(&self.element).ok().map(Rect::from),
        };

        let stage_ask = asking_started.elapsed();

        if let Some(current) = current_position {
            had_size = (current.right, current.bottom);

            if current.right == rect.right && current.bottom == rect.bottom {
                size_already_correct = true;

                // Skipped only when komorebi is the one who put it there.
                //
                // A frame read back from the application is not evidence that the window
                // is on screen. After waking from sleep the windows report exactly the
                // coordinates they had before -- measured, all four of them -- while
                // nothing is drawn at those coordinates, so komorebi decided there was
                // nothing to do and the workspace stayed empty until the user navigated
                // away and back.
                //
                // What komorebi remembers is different in kind: it means "I placed this
                // window here, and nothing has moved it since". That record is dropped
                // whenever anything else touches the window, and it does not exist at all
                // before komorebi has placed the window once -- which is exactly the
                // situation where the frame cannot be trusted.
                if known.is_some() && current.left == rect.left && current.top == rect.top {
                    tracing::debug!("SELFMOVE skip window={} (placed here by komorebi)", self.id);
                    return Ok(());
                }
            }
        }


        // Check if animation is enabled (per-animation or global)
        let animation_enabled = {
            let per_animation = ANIMATION_ENABLED_PER_ANIMATION.lock();
            per_animation
                .get(&MovementRenderDispatcher::PREFIX)
                .copied()
                .unwrap_or_else(|| ANIMATION_ENABLED_GLOBAL.load(Ordering::SeqCst))
        };

        // Say in advance what this move is about to make macOS report back, so those
        // reports are recognised as komorebi's own rather than as the window having moved
        // by itself. A position write reports one move; a size write reports a resize too.
        expect_self_move_echoes(self.id, if size_already_correct { 1 } else { 2 });

        let started = std::time::Instant::now();

        let result = if animation_enabled {
            self.set_position_animated(rect)
        } else {
            self.set_position_direct(rect, size_already_correct)
        };

        // TIMING: the write on its own, before the read-back below is added to it. One
        // application dominates every workspace it is on -- WhatsApp costs 102ms against
        // 5ms for Code -- and the two halves have different answers: a slow write is the
        // application taking its time to move, a slow read is komorebi asking it a
        // question it did not need to ask.
        let stage_write = started.elapsed();

        // DIAGNOSTIC: an app is free to refuse the geometry we ask for -- most
        // commonly because the rect is below its minimum window size, which is
        // what makes a window overflow its grid cell on denser layouts. Nothing
        // currently notices: set_position reports success as long as the AX call
        // itself succeeded, never that the window ignored it. Read the geometry
        // back and report the difference. Grep marker: SELFMOVE mismatch.
        let application = self.application.name();

        // Whether the read-back below ran, and what it found. `None` means it did not run.
        let mut landed_where_asked: Option<bool> = None;

        // Only ask where it landed while the answer is still unknown. See
        // [`crate::min_size::worth_verifying`].
        let worth_verifying = application
            .as_deref()
            .is_none_or(|application| {
                crate::min_size::worth_verifying(application, rect.right, rect.bottom)
            });

        if result.is_ok()
            && worth_verifying
            && let Ok(actual) = MacosApi::window_rect(&self.element)
        {
            let actual = Rect::from(actual);

            // Remember where it actually landed, so the next pass needs no round trip.
            // This is the accurate answer -- read from the window itself -- and it wins
            // over the assumption made below.
            CONFIRMED_POSITIONS.lock().insert(self.id, actual);
            landed_where_asked = Some(actual == *rect);

            if actual.right == rect.right && actual.bottom == rect.bottom {
                if let Some(application) = &application {
                    crate::min_size::note_accepted(application, actual.right, actual.bottom);
                }
            } else {
                tracing::warn!(
                    "SELFMOVE mismatch window={} asked={}x{} got={}x{} (delta {}x{})",
                    self.id,
                    rect.right,
                    rect.bottom,
                    actual.right,
                    actual.bottom,
                    actual.right - rect.right,
                    actual.bottom - rect.bottom
                );

                // Refusing to get smaller is the app telling us its minimum. Remember
                // it so the layout can route around it next time instead of
                // rediscovering it by overlapping windows again. Either dimension can
                // be the one refused, and measurement says height is the more common
                // of the two: a dense grid runs out of rows before it runs out of
                // columns. Zero means "nothing to report about this dimension".
                if let Some(name) = &application {
                    crate::min_size::forget_accepted(name);

                    let refused_width = if actual.right > rect.right {
                        actual.right
                    } else {
                        0
                    };

                    let refused_height = if actual.bottom > rect.bottom {
                        actual.bottom
                    } else {
                        0
                    };

                    if refused_width > 0 || refused_height > 0 {
                        crate::min_size::record(name, refused_width, refused_height);
                    }
                }
            }
        }

        // Remember where it was put.
        //
        // Only when the read-back above did not run, and so has not already recorded the
        // real answer. It skips itself once an application has been seen to accept a size
        // at least this small, which is the same as saying there is nothing left for it
        // to refuse -- so what was asked for is what it got.
        if result.is_ok() && landed_where_asked.is_none() {
            CONFIRMED_POSITIONS.lock().insert(self.id, *rect);
        }

        // TIMING: one window placement, the unit of work everything else multiplies.
        //
        // Split, because the two halves have different answers. The write is the
        // application taking its time to move and there is little to be done about it.
        // The read is komorebi asking where the window ended up -- a question it asks of
        // the same busy process it has just finished waiting for, and one it only needs
        // to ask while it is still learning what that application will accept.
        let elapsed = started.elapsed();
        if elapsed.as_millis() >= 2 {
            tracing::warn!(
                "TIMING set_position window={} app={:?} took={}ms ask={}ms write={}ms readback={}ms resized={} had={}x{} want={}x{}",
                self.id,
                application.unwrap_or_default(),
                elapsed.as_millis(),
                stage_ask.as_millis(),
                stage_write.as_millis(),
                elapsed.saturating_sub(stage_write).as_millis(),
                !size_already_correct,
                had_size.0,
                had_size.1,
                rect.right,
                rect.bottom
            );
        }

        result
    }

    fn set_position_direct(
        &self,
        rect: &Rect,
        size_already_correct: bool,
    ) -> Result<(), AccessibilityError> {
        // Disable AXEnhancedUserInterface during the move: when it's on (macOS
        // enables it when an accessibility client connects), the app animates
        // the position/size change with its own implicit animation (~200ms),
        // independent and outside our control. That causes the staggered
        // rendering when switching spaces. With EUI off the move is instant
        // and synchronous.
        let _enhanced_ui = self.hold_enhanced_ui_off();

        self.set_point(
            CGPoint::new(rect.left as CGFloat, rect.top as CGFloat),
            true,
        )?;

        if size_already_correct {
            return Ok(());
        }

        self.set_size(
            CGSize::new(rect.right as CGFloat, rect.bottom as CGFloat),
            true,
        )
    }

    /// Hold this window's application out of its own move animations for as long as the
    /// returned value lives. See [`crate::accessibility::private::hold_enhanced_ui_off`].
    pub fn hold_enhanced_ui_off(&self) -> EnhancedUiHeldOff<'_> {
        crate::accessibility::private::hold_enhanced_ui_off(
            self.application.process_id,
            self.application.element(),
        )
    }

    fn set_position_animated(&self, target_rect: &Rect) -> Result<(), AccessibilityError> {
        // Get current window position
        let current_rect = Rect::from(MacosApi::window_rect(&self.element)?);

        // If already at target position, skip animation
        if current_rect == *target_rect {
            return Ok(());
        }

        // Get animation style (per-animation or global)
        let style = {
            let per_animation = ANIMATION_STYLE_PER_ANIMATION.lock();
            per_animation
                .get(&MovementRenderDispatcher::PREFIX)
                .copied()
                .unwrap_or_else(|| *ANIMATION_STYLE_GLOBAL.lock())
        };

        // Get animation duration (per-animation or global)
        let duration = {
            let per_animation = ANIMATION_DURATION_PER_ANIMATION.lock();
            per_animation
                .get(&MovementRenderDispatcher::PREFIX)
                .copied()
                .unwrap_or_else(|| ANIMATION_DURATION_GLOBAL.load(Ordering::SeqCst))
        };

        // Create render dispatcher
        let dispatcher = MovementRenderDispatcher::new(
            self.id,
            self.element.clone(),
            self.observer.clone(),
            current_rect,
            *target_rect,
            style,
        );

        // Run animation (AnimationEngine handles cancellation and registration internally)
        let duration = Duration::from_millis(duration);
        if let Err(e) = AnimationEngine::animate(dispatcher, duration) {
            tracing::warn!("Animation failed for window {}: {}", self.id, e);
            // Fall back to direct positioning. The size is set unconditionally here:
            // an animation that failed part-way leaves the window at an unknown size.
            return self.set_position_direct(target_rect, false);
        }

        Ok(())
    }

    pub fn focus(&self, mouse_follows_focus: bool) -> Result<(), LibraryError> {
        // Remember that this focus change is ours, so the focus event it produces can be
        // recognised as an echo rather than as the user going somewhere.
        crate::workspace_reconciliator::note_focus_we_caused(self.id);

        // TIMING: focusing one window costs as much as laying out a whole workspace --
        // 43ms measured, against 40ms for placing every window on it. Three different
        // things happen in here and only measurement says which one it is.
        let focus_started = std::time::Instant::now();

        match self.running_application() {
            Ok(running_application) => {
                running_application.activateWithOptions(NSApplicationActivationOptions::empty());
            }
            Err(error) => {
                tracing::warn!(
                    "failed to get running application for {} ({:?}): {error}",
                    self.application.process_id,
                    self.application.name()
                );
            }
        }

        let stage_activate = focus_started.elapsed();
        let t = std::time::Instant::now();

        let cf_boolean = CFBoolean::new(true);
        let value = &**cf_boolean;

        // For tabbed applications, use the current main window element instead of
        // the stored element, which may point to a different tab
        let tabbed_applications = TABBED_APPLICATIONS.lock();
        let is_tabbed = tabbed_applications.contains(&self.application.name().unwrap_or_default());
        drop(tabbed_applications);

        let element_to_focus = if is_tabbed {
            // For tabbed apps, we still need to check if the stored element is valid
            // If valid, use it directly (allows focusing specific windows)
            // If invalid (tab was closed), fall back to main_window() for the active tab
            match AccessibilityApi::window_id(&self.element.0) {
                Ok(_) => self.element.clone(), // Element is valid, use it
                Err(_) => {
                    // Element is invalid (tab closed), get current main window
                    self.application
                        .main_window()
                        .map(AccessibilityUiElement)
                        .unwrap_or_else(|| self.element.clone())
                }
            }
        } else {
            self.element.clone()
        };

        let stage_element = t.elapsed();
        let t = std::time::Instant::now();

        AccessibilityApi::set_attribute_cf_value(&element_to_focus, kAXMainAttribute, value)?;

        let stage_main = t.elapsed();
        let t = std::time::Instant::now();

        if mouse_follows_focus {
            // Same story as hiding: this read is 44ms of WhatsApp's 67ms focus, spent
            // asking where a window is in order to put the pointer in the middle of it.
            //
            // Only when the element being focused is this window's own. A tabbed
            // application can hand back a different element above, and what komorebi
            // remembers is about this one.
            let known = if is_tabbed {
                None
            } else {
                CONFIRMED_POSITIONS.lock().get(&self.id).copied()
            };

            let rect = match known {
                Some(known) => known,
                None => MacosApi::window_rect(&element_to_focus)?.into(),
            };

            MacosApi::center_cursor_in_rect(&rect)?
        }

        let elapsed = focus_started.elapsed();
        if elapsed.as_millis() >= 2 {
            tracing::warn!(
                "TIMING focus window={} app={:?} took={}ms activate={}ms element={}ms main={}ms cursor={}ms",
                self.id,
                self.application.name().unwrap_or_default(),
                elapsed.as_millis(),
                stage_activate.as_millis(),
                stage_element.as_millis(),
                stage_main.as_millis(),
                t.elapsed().as_millis()
            );
        }

        Ok(())
    }

    pub fn raise(&self) -> Result<(), AccessibilityError> {
        let cf_boolean = CFBoolean::new(true);
        let value = &**cf_boolean;
        AccessibilityApi::set_attribute_cf_value(&self.element, kAXMainAttribute, value)?;
        AccessibilityApi::set_attribute_cf_value(&self.element, kAXFocusedAttribute, value)
    }

    pub fn set_point(&self, point: CGPoint, should_reap: bool) -> Result<(), AccessibilityError> {
        let result = AccessibilityApi::set_attribute_ax_value(
            &self.element,
            kAXPositionAttribute,
            AXValueType::CGPoint,
            point,
        );

        if should_reap {
            reaper::notify_on_error(self, result)
        } else {
            result
        }
    }

    pub fn set_size(&self, size: CGSize, should_reap: bool) -> Result<(), AccessibilityError> {
        let result = AccessibilityApi::set_attribute_ax_value(
            &self.element,
            kAXSizeAttribute,
            AXValueType::CGSize,
            size,
        );

        if should_reap {
            reaper::notify_on_error(self, result)
        } else {
            result
        }
    }

    pub fn center(&mut self, work_area: &Rect, resize: bool) -> Result<(), AccessibilityError> {
        let (target_width, target_height) = if resize {
            let (aspect_ratio_width, aspect_ratio_height) = FLOATING_WINDOW_TOGGLE_ASPECT_RATIO
                .lock()
                .width_and_height();
            let target_height = work_area.bottom / 2;
            let target_width = (target_height * aspect_ratio_width) / aspect_ratio_height;
            (target_width, target_height)
        } else {
            let current_rect = Rect::from(MacosApi::window_rect(&self.element)?);
            (current_rect.right, current_rect.bottom)
        };

        let x = work_area.left + ((work_area.right - target_width) / 2);
        let y = work_area.top + ((work_area.bottom - target_height) / 2);

        self.set_position(&Rect {
            left: x,
            top: y,
            right: target_width,
            bottom: target_height,
        })
    }

    pub fn move_to_area(
        &self,
        current_area: &Rect,
        target_area: &Rect,
    ) -> Result<(), AccessibilityError> {
        let current_rect = Rect::from(MacosApi::window_rect(&self.element)?);
        let x_diff = target_area.left - current_area.left;
        let y_diff = target_area.top - current_area.top;
        let x_ratio = f32::abs((target_area.right as f32) / (current_area.right as f32));
        let y_ratio = f32::abs((target_area.bottom as f32) / (current_area.bottom as f32));
        let window_relative_x = current_rect.left - current_area.left;
        let window_relative_y = current_rect.top - current_area.top;
        let corrected_relative_x = (window_relative_x as f32 * x_ratio) as i32;
        let corrected_relative_y = (window_relative_y as f32 * y_ratio) as i32;
        let window_x = current_area.left + corrected_relative_x;
        let window_y = current_area.top + corrected_relative_y;
        let left = x_diff + window_x;
        let top = y_diff + window_y;

        let corrected_width = (current_rect.right as f32 * x_ratio) as i32;
        let corrected_height = (current_rect.bottom as f32 * y_ratio) as i32;

        let new_rect = Rect {
            left,
            top,
            right: corrected_width,
            bottom: corrected_height,
        };

        // TODO: figure out what to do about maximized windows on macOS
        // let is_maximized = &new_rect == target_area;
        // if is_maximized {
        //     windows_api::WindowsApi::unmaximize_window(self.hwnd);
        //     let animation_enabled = ANIMATION_ENABLED_PER_ANIMATION.lock();
        //     let move_enabled = animation_enabled
        //         .get(&MovementRenderDispatcher::PREFIX)
        //         .is_some_and(|v| *v);
        //     drop(animation_enabled);
        //
        //     if move_enabled || ANIMATION_ENABLED_GLOBAL.load(Ordering::SeqCst) {
        //         let anim_count = ANIMATION_MANAGER
        //             .lock()
        //             .count_in_progress(MovementRenderDispatcher::PREFIX);
        //         self.set_position(&new_rect, true)?;
        //         let hwnd = self.hwnd;
        //         // Wait for the animation to finish before maximizing the window again, otherwise
        //         // we would be maximizing the window on the current monitor anyway
        //         thread::spawn(move || {
        //             let mut new_anim_count = ANIMATION_MANAGER
        //                 .lock()
        //                 .count_in_progress(MovementRenderDispatcher::PREFIX);
        //             let mut max_wait = 2000; // Max waiting time. No one will be using an animation longer than 2s, right? RIGHT??? WHY?
        //             while new_anim_count > anim_count && max_wait > 0 {
        //                 thread::sleep(Duration::from_millis(10));
        //                 new_anim_count = ANIMATION_MANAGER
        //                     .lock()
        //                     .count_in_progress(MovementRenderDispatcher::PREFIX);
        //                 max_wait -= 1;
        //             }
        //             windows_api::WindowsApi::maximize_window(hwnd);
        //         });
        //     } else {
        //         self.set_position(&new_rect, true)?;
        //         windows_api::WindowsApi::maximize_window(self.hwnd);
        //     }
        // } else {
        self.set_position(&new_rect)?;
        // }

        Ok(())
    }

    #[tracing::instrument(skip_all)]
    /// Whether the user has explicitly asked for this window to be tiled.
    fn matches_manage_rules(&self) -> bool {
        let (Some(title), Some(exe), Some(role), Some(subrole), Some(path)) = (
            self.title(),
            self.exe(),
            self.role(),
            self.subrole(),
            self.path(),
        ) else {
            return false;
        };

        let manage_identifiers = MANAGE_IDENTIFIERS.lock();
        let regex_identifiers = REGEX_IDENTIFIERS.lock();

        should_act(
            &title,
            &exe,
            &[&role, &subrole],
            &path.to_string_lossy(),
            &manage_identifiers,
            &regex_identifiers,
        )
        .is_some()
    }

    pub fn should_manage(
        &self,
        event: Option<WindowManagerEvent>,
        debug: &mut RuleDebug,
    ) -> eyre::Result<bool> {
        let decision = self.should_manage_inner(event, debug);

        // TRACE: what komorebi decided to tile, and how the window described itself when
        // it decided. Things that are not really windows keep finding their way into the
        // grid -- the Notification Center, Raycast, the login window, Brave's translation
        // bubble -- and every time, all that was needed to exclude them was knowing what
        // they call themselves.
        //
        // Reported here rather than at a call site, which is what made the last one so
        // hard to see: the trace sat on one of the paths that manages a window, the
        // bubble arrived by another, and the log said nothing was managed while a quarter
        // of the workspace was given to it. Every path goes through this function.
        // Grep marker: MANAGING.
        if matches!(decision, Ok(true)) && MANAGE_LOGGED.lock().insert(self.id) {
            tracing::warn!(
                "MANAGING window={} app={:?} role={:?} subrole={:?} title={:?}",
                self.id,
                self.application.name().unwrap_or_default(),
                self.role().unwrap_or_default(),
                self.subrole().unwrap_or_default(),
                self.title().unwrap_or_default()
            );
        }

        decision
    }

    fn should_manage_inner(
        &self,
        event: Option<WindowManagerEvent>,
        debug: &mut RuleDebug,
    ) -> eyre::Result<bool> {
        if !self.is_valid() {
            return Ok(false);
        }

        debug.is_window = true;

        // let rect = Rect::from(MacosApi::window_rect(&self.element).unwrap_or_default());
        //
        // if rect.right < MINIMUM_WIDTH.load(Ordering::SeqCst) {
        //     return Ok(false);
        // }
        //
        // debug.has_minimum_width = true;
        //
        // if rect.bottom < MINIMUM_HEIGHT.load(Ordering::SeqCst) {
        //     return Ok(false);
        // }
        //
        // debug.has_minimum_height = true;

        // System panels are not application windows.
        //
        // Nothing checked the subrole, so anything with a title was fair game: the
        // Notification Center identifies itself as AXSystemDialog, carries the title
        // "Notification Center", and was being tiled -- taking half a workspace and
        // leaving the desktop showing through, because there is nothing there to draw.
        // Raycast is the same shape. These are panels the system puts on top of things,
        // like Spotlight, and they belong outside the layout.
        //
        // manage_rules still wins, for anything that genuinely wants to be tiled.
        // AXUnknown belongs here too, and for the same reason.
        //
        // A window that declines to say what kind of window it is, is not an application
        // window. Two turned up: the macOS login window, which was being tiled into the
        // grid while the screen was locked, and Brave's "translate this page?" bubble,
        // which took a quarter of the workspace and showed the desktop through the rest
        // of it -- because there is only a small popup there to draw.
        //
        // The distinction is not cosmetic: these are transient things the system or the
        // application puts on top, and giving them a cell means the layout is built
        // around something that is about to disappear.
        if self
            .subrole()
            .is_some_and(|subrole| subrole == "AXSystemDialog" || subrole == "AXUnknown")
            && !self.matches_manage_rules()
        {
            return Ok(false);
        }

        // A window that has only just been created has not been given a title yet.
        //
        // The check below rejects untitled windows, which is right for the invisible
        // helper elements applications keep around -- but a window announcing its own
        // creation is a different thing. Ghostty titles a new window once the shell
        // starts, seconds later; until then this rejected it, so a window opened with
        // Cmd+N never entered the layout and komorebi only noticed it when a later click
        // produced a focus event. Applications whose windows are titled from birth (VS
        // Code among them) passed, which is why it looked app-specific.
        // ...and only for an ordinary window.
        //
        // Without this the exception also let in launcher panels, which report an empty
        // title too: Raycast started being tiled into the grid instead of floating over
        // it like Spotlight. Those identify themselves as AXSystemDialog rather than
        // AXStandardWindow, which separates "a window that has not been named yet" from
        // "a panel that never will be" without naming applications one by one.
        let is_newly_created = matches!(
            event,
            Some(WindowManagerEvent::Show(
                SystemNotification::Accessibility(AccessibilityNotification::AXWindowCreated),
                _,
            ))
        ) && self
            .subrole()
            .is_some_and(|subrole| subrole == "AXStandardWindow");

        let titleless_is_expected = |window: &Self| {
            is_newly_created
                || TITLELESS_APPLICATIONS
                    .lock()
                    .contains(&window.exe().unwrap_or_default())
        };

        match self.title() {
            None => {
                if titleless_is_expected(self) {
                    debug.matches_titleless_applications = self.exe();
                } else {
                    return Ok(false);
                }
            }
            Some(title) => {
                // Raycast is dumb and reports an empty string as a title
                if title.is_empty() {
                    if titleless_is_expected(self) {
                        debug.matches_titleless_applications = self.exe();
                    } else {
                        return Ok(false);
                    }
                } else {
                    debug.has_title = true;
                }
            }
        }

        // let is_cloaked = self.is_cloaked().unwrap_or_default();
        //
        // debug.is_cloaked = is_cloaked;

        // let mut allow_cloaked = false;

        // if let Some(event) = event {
        //     if matches!(
        //         event,
        //         WindowManagerEvent::Hide(_, _) | WindowManagerEvent::Cloak(_, _)
        //     ) {
        //         allow_cloaked = true;
        //     }
        // }

        // debug.allow_cloaked = allow_cloaked;

        // match (allow_cloaked, is_cloaked) {
        //     // If allowing cloaked windows, we don't need to check the cloaked status
        //     (true, _) |
        //     // If not allowing cloaked windows, we need to ensure the window is not cloaked
        //     (false, false) => {
        let title = if debug.matches_titleless_applications.is_some() {
            self.exe()
        } else {
            self.title()
        };

        if let (Some(title), Some(exe_name), Some(role), Some(subrole), Some(path)) =
            (title, self.exe(), self.role(), self.subrole(), self.path())
        {
            debug.title = Some(title.clone());
            debug.exe_name = Some(exe_name.clone());
            debug.role = Some(role.clone());
            debug.subrole = Some(subrole.clone());
            debug.path = Some(path.to_string_lossy().to_string());
            // calls for styles can fail quite often for events with windows that aren't really "windows"
            // since we have moved up calls of should_manage to the beginning of the process_event handler,
            // we should handle failures here gracefully to be able to continue the execution of process_event
            // if let (Ok(style), Ok(ex_style)) = (&self.style(), &self.ex_style()) {
            //     debug.window_style = Some(*style);
            //     debug.extended_window_style = Some(*ex_style);
            let eligible = window_is_eligible(
                self.id,
                &title,
                &exe_name,
                &[&role, &subrole],
                &path.to_string_lossy(),
                event,
                debug,
            );
            // debug.should_manage = eligible;
            return Ok(eligible);
            // }
        }
        // }
        //     _ => {}
        // }
        Ok(false)
    }
}

pub struct AdhocWindow;

impl AdhocWindow {
    pub fn process_id(element: &CFRetained<AXUIElement>) -> Option<i32> {
        let mut process_id = 0;

        unsafe {
            element.pid(NonNull::from_mut(&mut process_id));
        }

        if process_id != 0 {
            Some(process_id)
        } else {
            None
        }
    }

    pub fn exe(element: &CFRetained<AXUIElement>) -> Option<String> {
        let parent =
            AccessibilityApi::copy_attribute_value::<AXUIElement>(element, kAXParentAttribute)?;

        AccessibilityApi::copy_attribute_value::<CFString>(&parent, kAXTitleAttribute)
            .map(|s| s.to_string())
    }

    pub fn role(element: &CFRetained<AXUIElement>) -> Option<String> {
        AccessibilityApi::copy_attribute_value::<CFString>(element, kAXRoleAttribute)
            .map(|s| s.to_string())
    }

    pub fn subrole(element: &CFRetained<AXUIElement>) -> Option<String> {
        AccessibilityApi::copy_attribute_value::<CFString>(element, kAXSubroleAttribute)
            .map(|s| s.to_string())
    }

    pub fn title(element: &CFRetained<AXUIElement>) -> Option<String> {
        AccessibilityApi::copy_attribute_value::<CFString>(element, kAXTitleAttribute)
            .map(|s| s.to_string())
    }

    pub fn minimize(element: &CFRetained<AXUIElement>) -> Result<(), AccessibilityError> {
        let cf_boolean = CFBoolean::new(true);
        let value = &**cf_boolean;
        AccessibilityApi::set_attribute_cf_value(element, kAXMinimizedAttribute, value)
    }

    pub fn close(element: &CFRetained<AXUIElement>) -> Result<(), AccessibilityError> {
        if let Some(close_button) =
            AccessibilityApi::copy_attribute_value::<AXUIElement>(element, kAXCloseButtonAttribute)
        {
            AccessibilityApi::perform_action(&close_button, kAXPressAction)
        } else {
            Ok(())
        }
    }

    #[tracing::instrument(skip_all)]
    pub fn hide(
        id: u32,
        element: &CFRetained<AXUIElement>,
        hiding_position: WindowHidingPosition,
    ) -> Result<(), AccessibilityError> {
        let mut window_restore_positions = WINDOW_RESTORE_POSITIONS.lock();
        if let Entry::Vacant(entry) = window_restore_positions.entry(id) {
            let rect = MacosApi::window_rect(element)?;
            if let Some(monitor_size) = CoreGraphicsApi::display_bounds_for_window_rect(rect) {
                entry.insert(rect);
                drop(window_restore_positions);

                // I don't love this, but it's basically what Aerospace does in lieu of an actual "Hide" API
                let hidden_rect = match hiding_position {
                    WindowHidingPosition::BottomLeft => {
                        hidden_frame_bottom_left(monitor_size, rect.size)
                    }
                    WindowHidingPosition::BottomRight => {
                        hidden_frame_bottom_right(monitor_size, rect.size)
                    }
                };

                tracing::debug!(
                    "hiding window with id {id} and setting restore point to {},{}",
                    rect.origin.x,
                    rect.origin.y,
                );

                AccessibilityApi::set_attribute_ax_value(
                    element,
                    kAXPositionAttribute,
                    AXValueType::CGPoint,
                    hidden_rect.origin,
                )?;

                AccessibilityApi::set_attribute_ax_value(
                    element,
                    kAXSizeAttribute,
                    AXValueType::CGSize,
                    hidden_rect.size,
                )?;
            }
        }

        Ok(())
    }

    #[tracing::instrument(skip_all)]
    pub fn restore(id: u32, element: &CFRetained<AXUIElement>) -> Result<(), AccessibilityError> {
        let mut should_remove_restore_position = false;
        let mut window_restore_positions = WINDOW_RESTORE_POSITIONS.lock();
        if let Some(cg_rect) = window_restore_positions.get(&id) {
            tracing::debug!("restoring window with id {id} to {cg_rect:?}",);

            AccessibilityApi::set_attribute_ax_value(
                element,
                kAXPositionAttribute,
                AXValueType::CGPoint,
                cg_rect.origin,
            )?;

            AccessibilityApi::set_attribute_ax_value(
                element,
                kAXSizeAttribute,
                AXValueType::CGSize,
                cg_rect.size,
            )?;

            should_remove_restore_position = true;
        }

        if should_remove_restore_position {
            window_restore_positions.remove(&id);
        }

        Ok(())
    }

    pub fn raise(element: &CFRetained<AXUIElement>) -> Result<(), AccessibilityError> {
        let cf_boolean = CFBoolean::new(true);
        let value = &**cf_boolean;
        AccessibilityApi::set_attribute_cf_value(element, kAXMainAttribute, value)?;
        AccessibilityApi::set_attribute_cf_value(element, kAXFocusedAttribute, value)
    }
}

impl From<&CFDictionary> for WindowInfo {
    fn from(value: &CFDictionary) -> Self {
        unsafe {
            Self {
                name: cf_dictionary_value::<CFString>(value, kCGWindowName)
                    .map(|s| s.as_ref().to_string())
                    .and_then(|s| if s.is_empty() { None } else { Some(s) }),
                owner_pid: cf_dictionary_value::<CFNumber>(value, kCGWindowOwnerPID)
                    .and_then(|s| s.as_ref().as_i32())
                    .expect("window must have an owner process id"),
                owner_name: cf_dictionary_value::<CFString>(value, kCGWindowOwnerName)
                    .map(|s| s.as_ref().to_string())
                    .expect("window must have an owner name"),
                alpha: cf_dictionary_value::<CFNumber>(value, kCGWindowAlpha)
                    .and_then(|s| s.as_ref().as_f32())
                    .expect("window must have an alpha value"),

                bounds: if let Some(dict) =
                    cf_dictionary_value::<CFDictionary>(value, kCGWindowBounds).as_ref()
                {
                    WindowBounds::from(dict.as_ref())
                } else {
                    Default::default()
                },
                window_id: cf_dictionary_value::<CFNumber>(value, kCGWindowNumber)
                    .and_then(|n| n.as_ref().as_i64())
                    .map(|n| n as u32),
            }
        }
    }
}

#[derive(Default, Debug, Copy, Clone)]
pub struct WindowBounds {
    pub height: f32,
    pub width: f32,
    pub x: f32,
    pub y: f32,
}

impl From<&CFDictionary> for WindowBounds {
    fn from(value: &CFDictionary) -> Self {
        unsafe {
            Self {
                height: cf_dictionary_value::<CFNumber>(
                    value,
                    &CFString::from_static_str("Height"),
                )
                .and_then(|val| val.as_ref().as_f32())
                .unwrap_or_default(),
                width: cf_dictionary_value::<CFNumber>(value, &CFString::from_static_str("Width"))
                    .and_then(|val| val.as_ref().as_f32())
                    .unwrap_or_default(),
                x: cf_dictionary_value::<CFNumber>(value, &CFString::from_static_str("X"))
                    .and_then(|val| val.as_ref().as_f32())
                    .unwrap_or_default(),
                y: cf_dictionary_value::<CFNumber>(value, &CFString::from_static_str("Y"))
                    .and_then(|val| val.as_ref().as_f32())
                    .unwrap_or_default(),
            }
        }
    }
}

#[derive(Copy, Clone, Debug, Display, EnumString, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(untagged)]
/// Aspect ratio for temporarily floating windows
pub enum AspectRatio {
    /// A predefined aspect ratio
    #[cfg_attr(feature = "schemars", schemars(title = "Predefined"))]
    Predefined(PredefinedAspectRatio),
    /// A custom W:H aspect ratio
    #[cfg_attr(feature = "schemars", schemars(title = "Custom"))]
    Custom(i32, i32),
}

impl Default for AspectRatio {
    fn default() -> Self {
        AspectRatio::Predefined(PredefinedAspectRatio::default())
    }
}

#[derive(Copy, Clone, Debug, Default, Display, EnumString, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
/// Predefined aspect ratio
pub enum PredefinedAspectRatio {
    /// 21:9
    Ultrawide,
    /// 16:9
    Widescreen,
    /// 4:3
    #[default]
    Standard,
}

impl AspectRatio {
    pub fn width_and_height(self) -> (i32, i32) {
        match self {
            AspectRatio::Predefined(predefined) => match predefined {
                PredefinedAspectRatio::Ultrawide => (21, 9),
                PredefinedAspectRatio::Widescreen => (16, 9),
                PredefinedAspectRatio::Standard => (4, 3),
            },
            AspectRatio::Custom(w, h) => (w, h),
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct RuleDebug {
    pub should_manage: bool,
    pub is_window: bool,
    // pub has_minimum_width: bool,
    // pub has_minimum_height: bool,
    pub has_title: bool,
    // pub is_cloaked: bool,
    // pub allow_cloaked: bool,
    // pub allow_layered_transparency: bool,
    // pub window_style: Option<WindowStyle>,
    // pub extended_window_style: Option<ExtendedWindowStyle>,
    pub title: Option<String>,
    pub exe_name: Option<String>,
    pub role: Option<String>,
    pub subrole: Option<String>,
    pub path: Option<String>,
    pub matches_permaignore_class: Option<String>,
    pub matches_ignore_identifier: Option<MatchingRule>,
    pub matches_managed_override: Option<MatchingRule>,
    // pub matches_layered_whitelist: Option<MatchingRule>,
    pub matches_floating_applications: Option<MatchingRule>,
    pub matches_titleless_applications: Option<String>,
    // pub matches_wsl2_gui: Option<String>,
    // pub matches_no_titlebar: Option<MatchingRule>,
}

#[allow(clippy::too_many_arguments)]
fn window_is_eligible(
    _window_id: u32,
    title: &str,
    exe_name: &str,
    classes: &[&str],
    path: &str,
    // style: &WindowStyle,
    // ex_style: &ExtendedWindowStyle,
    _event: Option<WindowManagerEvent>,
    debug: &mut RuleDebug,
) -> bool {
    {
        let permaignore_classes = PERMAIGNORE_CLASSES.lock();
        for class in classes {
            if permaignore_classes.contains(&class.to_string()) {
                debug.matches_permaignore_class = Some(class.to_string());
                return false;
            }
        }
    }

    let regex_identifiers = REGEX_IDENTIFIERS.lock();

    let ignore_identifiers = IGNORE_IDENTIFIERS.lock();
    let should_ignore = if let Some(rule) = should_act(
        title,
        exe_name,
        classes,
        path,
        &ignore_identifiers,
        &regex_identifiers,
    ) {
        debug.matches_ignore_identifier = Some(rule);
        true
    } else {
        false
    };

    let manage_identifiers = MANAGE_IDENTIFIERS.lock();
    let managed_override = if let Some(rule) = should_act(
        title,
        exe_name,
        classes,
        path,
        &manage_identifiers,
        &regex_identifiers,
    ) {
        debug.matches_managed_override = Some(rule);
        true
    } else {
        false
    };

    let floating_identifiers = FLOATING_APPLICATIONS.lock();
    if let Some(rule) = should_act(
        title,
        exe_name,
        classes,
        path,
        &floating_identifiers,
        &regex_identifiers,
    ) {
        debug.matches_floating_applications = Some(rule);
    }

    if should_ignore && !managed_override {
        return false;
    }

    // let layered_whitelist = LAYERED_WHITELIST.lock();
    // let mut allow_layered = if let Some(rule) = should_act(
    //     title,
    //     exe_name,
    //     class,
    //     path,
    //     &layered_whitelist,
    //     &regex_identifiers,
    // ) {
    //     debug.matches_layered_whitelist = Some(rule);
    //     true
    // } else {
    //     false
    // };
    //
    // let known_layered_hwnds = transparency_manager::known_hwnds();

    // allow_layered = if known_layered_hwnds.contains(&hwnd)
    //     // we always want to process hide events for windows with transparency, even on other
    //     // monitors, because we don't want to be left with ghost tiles
    //     || matches!(event, Some(WindowManagerEvent::Hide(_, _)))
    // {
    //     debug.allow_layered_transparency = true;
    //     true
    // } else {
    //     allow_layered
    // };
    //
    // let allow_wsl2_gui = {
    //     let wsl2_ui_processes = WSL2_UI_PROCESSES.lock();
    //     let allow = wsl2_ui_processes.contains(exe_name);
    //     if allow {
    //         debug.matches_wsl2_gui = Some(exe_name.clone())
    //     }
    //
    //     allow
    // };

    // let titlebars_removed = NO_TITLEBAR.lock();
    // let allow_titlebar_removed = if let Some(rule) = should_act(
    //     title,
    //     exe_name,
    //     class,
    //     path,
    //     &titlebars_removed,
    //     &regex_identifiers,
    // ) {
    //     debug.matches_no_titlebar = Some(rule);
    //     true
    // } else {
    //     false
    // };
    //
    // {
    //     let slow_application_identifiers = SLOW_APPLICATION_IDENTIFIERS.lock();
    //     let should_sleep = should_act(
    //         title,
    //         exe_name,
    //         class,
    //         path,
    //         &slow_application_identifiers,
    //         &regex_identifiers,
    //     )
    //         .is_some();
    //
    //     if should_sleep {
    //         std::thread::sleep(Duration::from_millis(
    //             SLOW_APPLICATION_COMPENSATION_TIME.load(Ordering::SeqCst),
    //         ));
    //     }
    // }

    // TODO: not sure about this new manage by default base case for macOS
    true
}

#[allow(clippy::cognitive_complexity, clippy::too_many_lines)]
pub fn should_act(
    title: &str,
    exe_name: &str,
    classes: &[&str],
    path: &str,
    identifiers: &[MatchingRule],
    regex_identifiers: &HashMap<String, Regex>,
) -> Option<MatchingRule> {
    let mut matching_rule = None;
    for rule in identifiers {
        match rule {
            MatchingRule::Simple(identifier) => {
                if should_act_individual(
                    title,
                    exe_name,
                    classes,
                    path,
                    identifier,
                    regex_identifiers,
                ) {
                    matching_rule = Some(rule.clone());
                };
            }
            MatchingRule::Composite(identifiers) => {
                let mut composite_results = vec![];
                for identifier in identifiers {
                    composite_results.push(should_act_individual(
                        title,
                        exe_name,
                        classes,
                        path,
                        identifier,
                        regex_identifiers,
                    ));
                }

                if composite_results.iter().all(|&x| x) {
                    matching_rule = Some(rule.clone());
                }
            }
        }
    }

    matching_rule
}

pub fn should_act_individual(
    title: &str,
    exe_name: &str,
    classes: &[&str],
    path: &str,
    identifier: &IdWithIdentifier,
    regex_identifiers: &HashMap<String, Regex>,
) -> bool {
    let mut should_act = false;

    let mut identifier = identifier.clone();
    identifier.id = identifier.id.replace(".exe", "");

    match identifier.matching_strategy {
        None | Some(MatchingStrategy::Legacy) => match identifier.kind {
            ApplicationIdentifier::Title => {
                if title.starts_with(&identifier.id) || title.ends_with(&identifier.id) {
                    should_act = true;
                }
            }
            ApplicationIdentifier::Class => {
                for class in classes {
                    if class.starts_with(&identifier.id) || class.ends_with(&identifier.id) {
                        should_act = true;
                    }
                }
            }
            ApplicationIdentifier::Exe => {
                if exe_name.eq(&identifier.id) {
                    should_act = true;
                }
            }
            ApplicationIdentifier::Path => {
                if path.eq(&identifier.id) {
                    should_act = true;
                }
            }
        },
        Some(MatchingStrategy::Equals) => match identifier.kind {
            ApplicationIdentifier::Title => {
                if title.eq(&identifier.id) {
                    should_act = true;
                }
            }
            ApplicationIdentifier::Class => {
                for class in classes {
                    if class.eq(&identifier.id) {
                        should_act = true;
                    }
                }
            }
            ApplicationIdentifier::Exe => {
                if exe_name.eq(&identifier.id) {
                    should_act = true;
                }
            }
            ApplicationIdentifier::Path => {
                if path.eq(&identifier.id) {
                    should_act = true;
                }
            }
        },
        Some(MatchingStrategy::DoesNotEqual) => match identifier.kind {
            ApplicationIdentifier::Title => {
                if !title.eq(&identifier.id) {
                    should_act = true;
                }
            }
            ApplicationIdentifier::Class => {
                for class in classes {
                    if !class.eq(&identifier.id) {
                        should_act = true;
                    }
                }
            }
            ApplicationIdentifier::Exe => {
                if !exe_name.eq(&identifier.id) {
                    should_act = true;
                }
            }
            ApplicationIdentifier::Path => {
                if !path.eq(&identifier.id) {
                    should_act = true;
                }
            }
        },
        Some(MatchingStrategy::StartsWith) => match identifier.kind {
            ApplicationIdentifier::Title => {
                if title.starts_with(&identifier.id) {
                    should_act = true;
                }
            }
            ApplicationIdentifier::Class => {
                for class in classes {
                    if class.starts_with(&identifier.id) {
                        should_act = true;
                    }
                }
            }
            ApplicationIdentifier::Exe => {
                if exe_name.starts_with(&identifier.id) {
                    should_act = true;
                }
            }
            ApplicationIdentifier::Path => {
                if path.starts_with(&identifier.id) {
                    should_act = true;
                }
            }
        },
        Some(MatchingStrategy::DoesNotStartWith) => match identifier.kind {
            ApplicationIdentifier::Title => {
                if !title.starts_with(&identifier.id) {
                    should_act = true;
                }
            }
            ApplicationIdentifier::Class => {
                for class in classes {
                    if !class.starts_with(&identifier.id) {
                        should_act = true;
                    }
                }
            }
            ApplicationIdentifier::Exe => {
                if !exe_name.starts_with(&identifier.id) {
                    should_act = true;
                }
            }
            ApplicationIdentifier::Path => {
                if !path.starts_with(&identifier.id) {
                    should_act = true;
                }
            }
        },
        Some(MatchingStrategy::EndsWith) => match identifier.kind {
            ApplicationIdentifier::Title => {
                if title.ends_with(&identifier.id) {
                    should_act = true;
                }
            }
            ApplicationIdentifier::Class => {
                for class in classes {
                    if class.ends_with(&identifier.id) {
                        should_act = true;
                    }
                }
            }
            ApplicationIdentifier::Exe => {
                if exe_name.ends_with(&identifier.id) {
                    should_act = true;
                }
            }
            ApplicationIdentifier::Path => {
                if path.ends_with(&identifier.id) {
                    should_act = true;
                }
            }
        },
        Some(MatchingStrategy::DoesNotEndWith) => match identifier.kind {
            ApplicationIdentifier::Title => {
                if !title.ends_with(&identifier.id) {
                    should_act = true;
                }
            }
            ApplicationIdentifier::Class => {
                for class in classes {
                    if !class.ends_with(&identifier.id) {
                        should_act = true;
                    }
                }
            }
            ApplicationIdentifier::Exe => {
                if !exe_name.ends_with(&identifier.id) {
                    should_act = true;
                }
            }
            ApplicationIdentifier::Path => {
                if !path.ends_with(&identifier.id) {
                    should_act = true;
                }
            }
        },
        Some(MatchingStrategy::Contains) => match identifier.kind {
            ApplicationIdentifier::Title => {
                if title.contains(&identifier.id) {
                    should_act = true;
                }
            }
            ApplicationIdentifier::Class => {
                for class in classes {
                    if class.contains(&identifier.id) {
                        should_act = true;
                    }
                }
            }
            ApplicationIdentifier::Exe => {
                if exe_name.contains(&identifier.id) {
                    should_act = true;
                }
            }
            ApplicationIdentifier::Path => {
                if path.contains(&identifier.id) {
                    should_act = true;
                }
            }
        },
        Some(MatchingStrategy::DoesNotContain) => match identifier.kind {
            ApplicationIdentifier::Title => {
                if !title.contains(&identifier.id) {
                    should_act = true;
                }
            }
            ApplicationIdentifier::Class => {
                for class in classes {
                    if !class.contains(&identifier.id) {
                        should_act = true;
                    }
                }
            }
            ApplicationIdentifier::Exe => {
                if !exe_name.contains(&identifier.id) {
                    should_act = true;
                }
            }
            ApplicationIdentifier::Path => {
                if !path.contains(&identifier.id) {
                    should_act = true;
                }
            }
        },
        Some(MatchingStrategy::Regex) => match identifier.kind {
            ApplicationIdentifier::Title => {
                if let Some(re) = regex_identifiers.get(&identifier.id)
                    && re.is_match(title)
                {
                    should_act = true;
                }
            }
            ApplicationIdentifier::Class => {
                if let Some(re) = regex_identifiers.get(&identifier.id) {
                    for class in classes {
                        if re.is_match(class) {
                            should_act = true;
                        }
                    }
                }
            }
            ApplicationIdentifier::Exe => {
                if let Some(re) = regex_identifiers.get(&identifier.id)
                    && re.is_match(exe_name)
                {
                    should_act = true;
                }
            }
            ApplicationIdentifier::Path => {
                if let Some(re) = regex_identifiers.get(&identifier.id)
                    && re.is_match(path)
                {
                    should_act = true;
                }
            }
        },
    }

    should_act
}
