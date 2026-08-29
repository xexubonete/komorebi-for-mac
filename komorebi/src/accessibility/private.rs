#![allow(non_upper_case_globals, unused)]

use crate::accessibility::AccessibilityApi;
use crate::accessibility::error::AccessibilityError;
use objc2_application_services::AXError;
use objc2_application_services::AXUIElement;
use objc2_core_foundation::CFBoolean;
use objc2_core_graphics::CGWindowID;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::LazyLock;

pub const kAXEnhancedUserInterface: &str = "AXEnhancedUserInterface";

// this is the only private API call Aerospace uses, so I think we're ok to use it too
// https://github.com/nikitabobko/AeroSpace?tab=readme-ov-file#project-values
unsafe extern "C" {
    /// Extract `window_id` from an AXUIElement.
    pub fn _AXUIElementGetWindow(elem: &AXUIElement, window_id: *mut CGWindowID) -> AXError;
}

/// Get the current state of Enhanced User Interface for an element
pub fn get_enhanced_user_interface(element: &AXUIElement) -> bool {
    AccessibilityApi::copy_attribute_value::<CFBoolean>(element, kAXEnhancedUserInterface)
        .map(|b| b.as_bool())
        .unwrap_or(false)
}

/// Set the Enhanced User Interface state for an element
pub fn set_enhanced_user_interface(
    element: &AXUIElement,
    enabled: bool,
) -> Result<(), AccessibilityError> {
    let cf_boolean = CFBoolean::new(enabled);
    let value = &**cf_boolean;
    AccessibilityApi::set_attribute_cf_value(element, kAXEnhancedUserInterface, value)
}

/// Execute a closure with Enhanced User Interface temporarily disabled.
/// This can improve performance during window positioning operations.
pub fn with_enhanced_ui_disabled<F, R>(element: &AXUIElement, f: F) -> R
where
    F: FnOnce() -> R,
{
    let original_state = get_enhanced_user_interface(element);

    if original_state && let Err(error) = set_enhanced_user_interface(element, false) {
        tracing::warn!("Failed to disable Enhanced User Interface: {:?}", error);
    }

    let result = f();

    if original_state && let Err(error) = set_enhanced_user_interface(element, true) {
        tracing::warn!("Failed to restore Enhanced User Interface: {:?}", error);
    }

    result
}

/// Execute a closure with system-wide Enhanced User Interface temporarily disabled.
pub fn with_system_enhanced_ui_disabled<F, R>(f: F) -> R
where
    F: FnOnce() -> R,
{
    let system_element = unsafe { AXUIElement::new_system_wide() };
    with_enhanced_ui_disabled(&system_element, f)
}

/// Enhanced User Interface, per application, remembered and reference counted.
///
/// AXEnhancedUserInterface is an **application** attribute: macOS turns it on when an
/// accessibility client connects, and it makes the application animate every position and
/// size change with its own implicit animation, outside komorebi's control. Turning it off
/// around a move is what makes the move instant instead of a ~200ms slide.
///
/// Two things were being paid for on every single window placement:
///
/// * **A read.** The state was fetched again each time, for a value that only komorebi
///   ever changes. Read once per application and remembered.
/// * **Two writes per window.** Laying out a workspace moves several windows of the same
///   application one after another, turning the attribute off and on again around each.
///   The count means an outer scope can hold it off for a whole batch while the
///   per-window scopes inside cost nothing.
static ENHANCED_UI: LazyLock<Mutex<HashMap<i32, EnhancedUiState>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Copy, Clone)]
struct EnhancedUiState {
    /// What the application had before komorebi touched it. Only an application that had
    /// it on needs it turning back on.
    was_on: bool,
    /// How many live scopes are currently holding it off.
    holders: usize,
}

/// Hold Enhanced User Interface off for an application until this is dropped.
///
/// Nesting is safe: only the outermost scope does any work.
pub struct EnhancedUiHeldOff<'a> {
    process_id: i32,
    application: &'a AXUIElement,
}

pub fn hold_enhanced_ui_off(
    process_id: i32,
    application: &AXUIElement,
) -> EnhancedUiHeldOff<'_> {
    let mut known = ENHANCED_UI.lock();

    let state = known.entry(process_id).or_insert_with(|| {
        let was_on = get_enhanced_user_interface(application);

        // Once per application, and worth seeing: this attribute was previously being
        // read from the window element rather than the application's, where it does not
        // exist. Whether it reads as on here says whether the animations komorebi has
        // been trying to suppress were ever actually being suppressed.
        tracing::warn!("ENHANCED_UI process={process_id} was_on={was_on}");

        EnhancedUiState { was_on, holders: 0 }
    });

    let outermost = state.holders == 0;
    let was_on = state.was_on;
    state.holders += 1;
    drop(known);

    if outermost && was_on && let Err(error) = set_enhanced_user_interface(application, false) {
        tracing::warn!("could not disable Enhanced User Interface: {error:?}");
    }

    EnhancedUiHeldOff {
        process_id,
        application,
    }
}

impl Drop for EnhancedUiHeldOff<'_> {
    fn drop(&mut self) {
        let mut known = ENHANCED_UI.lock();

        let Some(state) = known.get_mut(&self.process_id) else {
            return;
        };

        state.holders = state.holders.saturating_sub(1);
        let outermost = state.holders == 0;
        let was_on = state.was_on;
        drop(known);

        if outermost
            && was_on
            && let Err(error) = set_enhanced_user_interface(self.application, true)
        {
            tracing::warn!("could not restore Enhanced User Interface: {error:?}");
        }
    }
}

/// An application komorebi will not manage again: stop remembering its state.
pub fn forget_enhanced_ui(process_id: i32) {
    ENHANCED_UI.lock().remove(&process_id);
}
