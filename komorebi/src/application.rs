use crate::AccessibilityObserver;
use crate::AccessibilityUiElement;
use crate::accessibility::AccessibilityApi;
use crate::accessibility::attribute_constants::kAXMainWindowAttribute;
use crate::accessibility::attribute_constants::kAXTitleAttribute;
use crate::accessibility::attribute_constants::kAXWindowsAttribute;
use crate::accessibility::error::AccessibilityError;
use crate::accessibility::notification_constants::AccessibilityNotification;
use crate::accessibility::notification_constants::kAXApplicationActivatedNotification;
use crate::accessibility::notification_constants::kAXApplicationDeactivatedNotification;
use crate::accessibility::notification_constants::kAXApplicationHiddenNotification;
use crate::accessibility::notification_constants::kAXApplicationShownNotification;
use crate::accessibility::notification_constants::kAXMainWindowChangedNotification;
use crate::accessibility::notification_constants::kAXUIElementDestroyedNotification;
use crate::accessibility::notification_constants::kAXWindowCreatedNotification;
use crate::window::Window;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::LazyLock;
use crate::window_manager_event::SystemNotification;
use crate::window_manager_event::WindowManagerEvent;
use crate::window_manager_event_listener;
use objc2_application_services::AXObserver;
use objc2_application_services::AXUIElement;
use objc2_core_foundation::CFArray;
use objc2_core_foundation::CFRetained;
use objc2_core_foundation::CFRunLoop;
use objc2_core_foundation::CFString;
use std::ffi::c_void;
use std::ptr::NonNull;
use std::str::FromStr;
use tracing::instrument;

const NOTIFICATIONS: &[&str] = &[
    kAXApplicationActivatedNotification,
    kAXApplicationDeactivatedNotification,
    kAXApplicationHiddenNotification,
    kAXApplicationShownNotification,
    // this is when we change focus between two windows of the same app
    kAXMainWindowChangedNotification,
    // this is when the same app has a new window opened
    kAXWindowCreatedNotification,
    // this is when a window of an application is destroyed / closed
    // when this fires, the app owner name won't be found, but the can be matched via PID
    kAXUIElementDestroyedNotification,
];
static APPLICATION_NAMES: LazyLock<Mutex<HashMap<i32, Option<String>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Forget everything remembered about a process: it has gone, and the next process to be
/// handed this id is a different application.
pub fn forget_application(process_id: i32) {
    APPLICATION_NAMES.lock().remove(&process_id);
    crate::accessibility::private::forget_enhanced_ui(process_id);
}


#[derive(Debug, Default, Clone, PartialEq)]
pub struct Application {
    element: AccessibilityUiElement,
    pub process_id: i32,
    pub observer: AccessibilityObserver,
    pub is_observable: bool,
}

#[instrument(skip_all)]
unsafe extern "C-unwind" fn application_observer_callback(
    _observer: NonNull<AXObserver>,
    element: NonNull<AXUIElement>,
    notification: NonNull<CFString>,
    _context: *mut c_void,
) {
    unsafe {
        let notification_str = notification.as_ref().to_string();


        let name =
            AccessibilityApi::copy_attribute_value::<CFString>(element.as_ref(), kAXTitleAttribute)
                .map(|s| s.to_string());

        // AXUIElementDestroyed fires when the element is already gone,
        // so we can't get the title - but we still need to process it
        let is_destroyed = matches!(
            AccessibilityNotification::from_str(&notification_str),
            Ok(AccessibilityNotification::AXUIElementDestroyed)
        );

        // And the mirror case: AXWindowCreated fires before the window has a title.
        //
        // The title check below exists to filter out the invisible helper elements apps
        // create, but a window announcing its own birth has not been given a title yet --
        // a terminal opened with Cmd+N gets one only once the shell starts. Requiring a
        // title here silently discarded every new window of an already-running app, so
        // komorebi never learned they existed: they stayed unmanaged and on top of the
        // layout, and nothing appeared in the log to say why.
        let is_created = matches!(
            AccessibilityNotification::from_str(&notification_str),
            Ok(AccessibilityNotification::AXWindowCreated)
        );

        if is_destroyed || is_created || name.as_ref().is_some_and(|n| !n.is_empty()) {
            let mut process_id = 0;
            element.as_ref().pid(NonNull::from_mut(&mut process_id));

            if let Ok(notification) = AccessibilityNotification::from_str(&notification_str)
                && let Some(event) = WindowManagerEvent::from_system_notification(
                    SystemNotification::Accessibility(notification),
                    process_id,
                    None,
                )
            {
                tracing::debug!(
                    "notification: {notification}, process: {process_id}, name: \"{}\"",
                    name.as_deref().unwrap_or("<destroyed>")
                );

                window_manager_event_listener::send_notification(event);
            }
        }
    }
}

impl Drop for Application {
    fn drop(&mut self) {
        // this gets called when an Application clone on a Window is dropped, so we need
        // to make sure it only invalidates the observer if the Application is no longer
        // running
        if self.is_observable && !self.is_valid() {
            tracing::info!(
                "invalidating application observer for process id {}",
                self.process_id
            );
            // make sure the observer gets removed from any run loops
            AccessibilityApi::invalidate_observer(&self.observer);
        }
    }
}

impl Application {
    pub fn new(process_id: i32) -> Result<Self, AccessibilityError> {
        Ok(Self {
            element: AccessibilityUiElement(AccessibilityApi::create_application(process_id)),
            process_id,
            observer: AccessibilityObserver(Some(AccessibilityApi::create_observer(
                process_id,
                Some(application_observer_callback),
            )?)),
            is_observable: true,
        })
    }

    /// The application's name, asked for once and remembered.
    ///
    /// This is a synchronous round trip to the other process, and it was being made
    /// dozens of times a second: every window placement asks for the name twice before
    /// it does anything, focusing asks again, and every log line that names an
    /// application asks once more. An application does not rename itself while it runs,
    /// so the answer is the same every time.
    ///
    /// Keyed by process id, and dropped when the process goes away (see
    /// [`forget_application`]) so that a reused pid cannot inherit a dead app's name.
    pub fn name(&self) -> Option<String> {
        if let Some(known) = APPLICATION_NAMES.lock().get(&self.process_id) {
            return known.clone();
        }

        let name = AccessibilityApi::copy_attribute_value::<CFString>(
            &self.element,
            kAXTitleAttribute,
        )
        .map(|s| s.to_string());

        // A miss is worth remembering too: an application that has no name yet is asked
        // over and over otherwise. It is forgotten below when anything about it changes.
        APPLICATION_NAMES
            .lock()
            .insert(self.process_id, name.clone());

        name
    }

    #[tracing::instrument(skip_all)]
    pub fn observe(&mut self, run_loop: &CFRunLoop, refcon: Option<*mut c_void>) {
        tracing::info!(
            "registering observer for process: {}, name: {}",
            self.process_id,
            self.name()
                .unwrap_or_else(|| String::from("<NO NAME FOUND>"))
        );

        let mut retries = 5;

        while retries > 0 {
            match AccessibilityApi::add_observer_to_run_loop(
                &self.observer,
                &self.element,
                NOTIFICATIONS,
                run_loop,
                refcon,
            ) {
                Ok(_) => {
                    self.is_observable = true;
                    break;
                }
                Err(error) => {
                    // Chromium apps are still gross on macOS too
                    retries -= 1;
                    std::thread::sleep(std::time::Duration::from_millis(100));
                    tracing::warn!(
                        "failed to register observer for process {} ({}): {error}, {retries} retries left",
                        self.process_id,
                        self.name()
                            .unwrap_or_else(|| String::from("<NO NAME FOUND>"))
                    );
                }
            }
        }
    }

    pub fn is_valid(&self) -> bool {
        AccessibilityApi::copy_attribute_names(&self.element).is_some()
    }

    /// The application's own accessibility element. Application-level attributes --
    /// AXEnhancedUserInterface among them -- have to be read and written on this, not on
    /// one of its windows.
    pub fn element(&self) -> &crate::AccessibilityUiElement {
        &self.element
    }

    pub fn window_elements(&self) -> Option<CFRetained<CFArray<AXUIElement>>> {
        AccessibilityApi::copy_attribute_value::<CFArray<AXUIElement>>(
            &self.element,
            kAXWindowsAttribute,
        )
    }

    pub fn main_window(&self) -> Option<CFRetained<AXUIElement>> {
        AccessibilityApi::copy_attribute_value::<AXUIElement>(&self.element, kAXMainWindowAttribute)
    }

    pub fn main_window_id(&self) -> Option<u32> {
        let window = AccessibilityApi::copy_attribute_value::<AXUIElement>(
            &self.element,
            kAXMainWindowAttribute,
        )?;
        AccessibilityApi::window_id(window.as_ref()).ok()
    }

    /// Finds the window whose Accessibility API window id matches the
    /// CGWindowList window number. Matching by id (not by title) lets us
    /// manage several windows of the same app even when they share a title
    /// (e.g. two Brave windows).
    pub fn window_by_id(&self, window_id: u32) -> Option<Window> {
        for element in self.window_elements()? {
            if let Ok(id) = AccessibilityApi::window_id(&element)
                && id == window_id
            {
                return Window::new(element, self.clone()).ok();
            }
        }

        None
    }

    pub fn window_by_title(&self, title: &str) -> Option<Window> {
        let mut target = None;

        for element in self.window_elements()? {
            let window = Window::new(element, self.clone()).ok()?;

            if let Some(window_title) = window.title()
                && (window_title.eq(title)
                    // a hack for handling already-launched apps with inconsistent titles when
                    // komorebi launches, such as Activity Monitor, Google Chrome, and counting...
                    || window_title.contains(title))
            {
                target = Some(window);
            }
        }

        target
    }
}
