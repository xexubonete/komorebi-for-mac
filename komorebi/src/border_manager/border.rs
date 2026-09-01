use crate::AccessibilityObserver;
use crate::AccessibilityUiElement;
use crate::CoreFoundationRunLoop;
use crate::accessibility::AccessibilityApi;
use crate::accessibility::notification_constants::kAXMainWindowChangedNotification;
use crate::accessibility::notification_constants::kAXWindowMovedNotification;
use crate::accessibility::notification_constants::kAXWindowResizedNotification;
use crate::border_manager::BORDER_OFFSET;
use crate::border_manager::BORDER_OFFSET_ADJUSTMENT;
use crate::border_manager::BORDER_WIDTH;
use crate::border_manager::ns_window::NsWindow;
use crate::border_manager::window_kind_colour;
use crate::core::Rect;
use crate::core::WindowKind;
use crate::core_graphics::CoreGraphicsApi;
use crate::macos_api::MacosApi;
use color_eyre::eyre;
use dispatch2::DispatchQueue;
use komorebi_themes::colour::Rgb;
use objc2::rc::Retained;
use objc2::rc::autoreleasepool;
use objc2_app_kit::NSWindow;
use objc2_application_services::AXObserver;
use objc2_application_services::AXUIElement;
use objc2_core_foundation::CFString;
use objc2_core_foundation::CGFloat;
use objc2_core_graphics::CGMainDisplayID;
use objc2_foundation::NSPoint;
use objc2_foundation::NSRect;
use objc2_quartz_core::CATransaction;
use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::atomic::Ordering;
use tracing::instrument;

#[instrument(skip_all)]
unsafe extern "C-unwind" fn border_observer_callback(
    _observer: NonNull<AXObserver>,
    _element: NonNull<AXUIElement>,
    _notification: NonNull<CFString>,
    context: *mut c_void,
) {
    unsafe {
        if !context.is_null() {
            let border = &*context.cast::<Border>();

            // Keep following the window even while hidden.
            //
            // This used to skip hidden borders as an optimisation, back when every window
            // wore one and hidden meant "about to be destroyed". Now only the focused
            // window shows a border, so most are hidden most of the time -- and a hidden
            // border that stops tracking its window comes back, when that window is next
            // focused, still drawn where the window used to be. Opening a second Ghostty
            // window is enough to see it: the layout reflows, the hidden borders ignore
            // it, and the next one shown is the wrong size.
            if let Ok(rect) = MacosApi::window_rect(&border.tracking_element) {
                let frame = Rect::from(CoreGraphicsApi::display_bounds(CGMainDisplayID()));
                let mut ns_rect = NSRect::new(
                    NSPoint::new(
                        rect.origin.x,
                        frame.bottom as CGFloat - rect.origin.y - rect.size.height,
                    ),
                    rect.size,
                );

                let offset =
                    BORDER_OFFSET.load(Ordering::Relaxed) as f64 + BORDER_OFFSET_ADJUSTMENT as f64;

                ns_rect.origin.x -= offset;
                ns_rect.origin.y -= offset;
                ns_rect.size.width += offset * 2.0;
                ns_rect.size.height += offset * 2.0;

                // TRACE: where the border is being drawn, against the window it is
                // meant to be drawn around. A border left over a window that has since
                // moved looks exactly like a border drawn wrong, and only these two
                // numbers side by side tell them apart. Grep marker: BORDERRECT.
                tracing::warn!(
                    "BORDERRECT window={} target={},{} {}x{} border={},{} {}x{}",
                    border.tracking_window_id,
                    rect.origin.x,
                    rect.origin.y,
                    rect.size.width,
                    rect.size.height,
                    ns_rect.origin.x,
                    ns_rect.origin.y,
                    ns_rect.size.width,
                    ns_rect.size.height
                );

                border.update();
                border
                    .ns_window
                    .window
                    .setFrame_display_animate(ns_rect, false, false);
            }
        }
    }
}

#[derive(Debug)]
pub struct Border {
    pub id: String,
    #[allow(dead_code)]
    // we need to keep a reference to this so it stays alive
    pub observer: AccessibilityObserver,
    pub tracking_element: AccessibilityUiElement,
    pub tracking_window_id: u32,
    pub process_id: i32,
    pub monitor_idx: Option<usize>,
    pub ns_window: NsWindow,
    pub window_kind: WindowKind,
    /// Which application this border frames, for looking up its corner radius.
    pub application_name: String,
}

unsafe impl Send for Border {}

impl Border {
    pub fn create(
        id: &str,
        tracking_window_id: u32,
        process_id: i32,
        element: AccessibilityUiElement,
        monitor_idx: Option<usize>,
        run_loop: CoreFoundationRunLoop,
    ) -> eyre::Result<Box<Self>> {
        let observer = AccessibilityObserver(Some(AccessibilityApi::create_observer(
            process_id,
            Some(border_observer_callback),
        )?));

        let rect = MacosApi::window_rect(&element).unwrap_or_default();

        let frame = Rect::from(CoreGraphicsApi::display_bounds(CGMainDisplayID()));
        let ns_rect = NSRect::new(
            NSPoint::new(
                rect.origin.x,
                frame.bottom as CGFloat - rect.origin.y - rect.size.height,
            ),
            rect.size,
        );

        let mut border = Box::new(Self {
            id: id.to_string(),
            tracking_window_id,
            process_id,
            monitor_idx,
            observer: observer.clone(),
            tracking_element: element.clone(),
            ns_window: NsWindow::new(ns_rect, tracking_window_id)?,
            window_kind: WindowKind::Unfocused,
            application_name: crate::application::Application::new(process_id)
                .ok()
                .and_then(|app| app.name())
                .unwrap_or_default(),
        });

        DispatchQueue::main().exec_sync(|| {
            let border_ptr = std::ptr::addr_of_mut!(*border).cast::<c_void>();
            if let Err(error) = AccessibilityApi::add_observer_to_run_loop(
                &observer,
                &element,
                &[
                    kAXWindowMovedNotification,
                    kAXWindowResizedNotification,
                    kAXMainWindowChangedNotification,
                ],
                &run_loop,
                Some(border_ptr),
            ) {
                tracing::warn!("failed to create border observer: {error}")
            }
        });

        Ok(border)
    }

    pub fn update_tracking_element(
        &mut self,
        new_element: AccessibilityUiElement,
        border_ptr: *mut c_void,
    ) {
        let old_element = &self.tracking_element;
        let notifications = [
            kAXWindowMovedNotification,
            kAXWindowResizedNotification,
            kAXMainWindowChangedNotification,
        ];

        // re-register observer notifications on the new element
        if let Some(ref observer) = self.observer.0 {
            for notification in &notifications {
                let _ = AccessibilityApi::remove_notification_from_observer(
                    observer,
                    &old_element.0,
                    notification,
                );
            }

            for notification in &notifications {
                let _ = AccessibilityApi::add_notification_to_observer(
                    observer,
                    &new_element.0,
                    notification,
                    Some(border_ptr),
                );
            }
        }

        self.tracking_element = new_element;
    }

    pub fn update(&self) {
        autoreleasepool(|_| {
            let colour = Rgb::from(window_kind_colour(self.window_kind));

            // Only the focused window wears a border.
            //
            // Borders sit above every ordinary window and there is no way to slot them
            // in between (see NsWindow::new), so each one is something that can cover a
            // window in front of it. Showing only the focused one takes that from one
            // per window down to exactly one -- and that one belongs to the window
            // which is, by definition, already in front.
            let visible = !matches!(
                self.window_kind,
                WindowKind::Unfocused | WindowKind::UnfocusedLocked
            );

            let appeared = self.ns_window.set_visible(visible);

            if !visible {
                return;
            }

            let width = BORDER_WIDTH.load(Ordering::Relaxed) as f64;

            CATransaction::begin();
            CATransaction::setDisableActions(true);
            self.ns_window.set_border_color(colour);
            self.ns_window.set_border_width(width);

            // Match the window's own rounding. macOS rounds windows differently per
            // application -- Apple's own use the system frame, Electron apps and custom
            // chrome draw their own -- and there is no way to ask a window what its radius
            // is, so it comes from the per-application rules.
            self.ns_window
                .set_corner_radius(crate::border_manager::border_radius_for(
                    &self.application_name,
                ) as f64);
            // TODO: why does this crash?
            // self.ns_window.window.setFrame_display(ns_rect, true);
            CATransaction::commit();

            // Flash only when the border has just come up. Now that only the focused
            // window carries one, a border appearing *is* its window taking focus --
            // and update() runs far too often to flash on every call. Outside the
            // CATransaction above, which disables actions and would swallow it.
            if appeared {
                self.ns_window.flash(
                    width,
                    crate::border_manager::flash_style(),
                    colour,
                );
            }
        })
    }

    /// Close the border window. Must already be on the main thread.
    ///
    /// The caller hops once and does both this and observer invalidation in that block;
    /// hopping again from here would deadlock.
    pub fn destroy_on_main_thread(&self) {
        let window_ptr = Retained::as_ptr(&self.ns_window.window) as usize;

        autoreleasepool(|_| unsafe {
            let window = window_ptr as *const NSWindow;
            (*window).close();
        });
    }
}
