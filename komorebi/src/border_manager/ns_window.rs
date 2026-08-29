use crate::border_manager::BORDER_OFFSET;
use crate::border_manager::BORDER_OFFSET_ADJUSTMENT;
use crate::border_manager::BORDER_RADIUS;
use crate::border_manager::BORDER_WIDTH;
use color_eyre::eyre;
use color_eyre::eyre::OptionExt;
use dispatch2::DispatchQueue;
use komorebi_themes::colour::Rgb;
use objc2::MainThreadMarker;
use objc2::MainThreadOnly;
use objc2::msg_send;
use objc2::rc::Retained;
use objc2::rc::autoreleasepool;
use objc2_app_kit::NSBackingStoreType;
use objc2_app_kit::NSColor;
use objc2_app_kit::NSFloatingWindowLevel;
use objc2_app_kit::NSView;
use objc2_app_kit::NSWindow;
use objc2_app_kit::NSWindowAnimationBehavior;
use objc2_app_kit::NSWindowCollectionBehavior;
use objc2_app_kit::NSWindowStyleMask;
use objc2_core_graphics::CGColor;
use objc2_foundation::NSDictionary;
use objc2_foundation::NSRect;
use objc2_foundation::NSNumber;
use objc2_foundation::NSString;
use objc2_quartz_core::CABasicAnimation;
use objc2_quartz_core::CALayer;
use objc2_quartz_core::CAMediaTimingFunction;
use std::ops::Deref;
use parking_lot::Mutex;
use std::sync::atomic::AtomicI32;
use std::sync::atomic::Ordering;
use std::sync::mpsc;

/// How the border announces that its window has taken focus.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum FlashStyle {
    /// Flares wide and settles back.
    ///
    /// The only style kept. Opacity, scale and colour were tried as well and none of
    /// them reach the screen: the border layer sits inside a transparent window with
    /// implicit actions disabled, and only its border width actually redraws. They
    /// animated correctly and were invisible, which is worse than not offering them.
    #[default]
    Width,
    /// No animation.
    None,
}

impl std::str::FromStr for FlashStyle {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "width" => Ok(Self::Width),
            "none" => Ok(Self::None),
            other => Err(format!("unknown flash style: {other}")),
        }
    }
}

/// Build a keyed animation. Split out because every style needs the same two lines.
unsafe fn basic_animation(key_path: &str) -> Retained<CABasicAnimation> {
    CABasicAnimation::animationWithKeyPath(Some(&NSString::from_str(key_path)))
}

/// How far the border flares, as a multiple of its settled width, times ten.
///
/// Stored scaled because these are set from config through atomics and a border width of
/// 3.5x reads better than forcing whole numbers.
pub static FLASH_FACTOR_X10: AtomicI32 = AtomicI32::new(35);

/// How long the flash lasts, in milliseconds.
pub static FLASH_DURATION_MS: AtomicI32 = AtomicI32::new(220);

/// Which Core Animation timing curve the flash follows.
static FLASH_EASING: Mutex<String> = Mutex::new(String::new());

/// Set the timing curve by name: easeOut (default), easeIn, easeInEaseOut, linear.
pub fn set_flash_easing(name: &str) {
    *FLASH_EASING.lock() = name.to_string();
}

fn flash_easing() -> String {
    let name = FLASH_EASING.lock().clone();
    if name.is_empty() {
        String::from("easeOut")
    } else {
        name
    }
}

#[derive(Debug)]
pub struct NsWindow {
    pub window: Retained<NSWindow>,
    pub layer: Retained<CALayer>,
}

unsafe impl Send for NsWindow {}

impl NsWindow {
    pub fn new(ns_rect: NSRect, target_window_id: u32) -> eyre::Result<NsWindow> {
        let offset = BORDER_OFFSET.load(Ordering::Relaxed) as f64 + BORDER_OFFSET_ADJUSTMENT as f64;

        let mut ns_rect = ns_rect;

        ns_rect.origin.x -= offset;
        ns_rect.origin.y -= offset;
        ns_rect.size.width += offset * 2.0;
        ns_rect.size.height += offset * 2.0;

        let (tx, rx) = mpsc::channel();

        DispatchQueue::main().exec_async(move || {
            autoreleasepool(|_| {
                let mtm = unsafe { MainThreadMarker::new_unchecked() };

                // Create window
                let window_frame = ns_rect;

                let window = unsafe {
                    let window = NSWindow::alloc(mtm);
                    NSWindow::initWithContentRect_styleMask_backing_defer(
                        window,
                        window_frame,
                        NSWindowStyleMask::Borderless,
                        NSBackingStoreType::Buffered,
                        false,
                    )
                };

                // Make transparent
                window.setBackgroundColor(Some(&NSColor::clearColor()));
                window.setAnimationBehavior(NSWindowAnimationBehavior::None);
                window.disableSnapshotRestoration();
                window.setPreservesContentDuringLiveResize(false);
                window.setRestorable(false);

                window.setHasShadow(false);
                window.setOpaque(false);
                window.setLevel(NSFloatingWindowLevel);
                window.setIgnoresMouseEvents(true);

                window.setCollectionBehavior(
                    NSWindowCollectionBehavior::CanJoinAllSpaces |
                        NSWindowCollectionBehavior::Stationary |
                        NSWindowCollectionBehavior::IgnoresCycle |
                        NSWindowCollectionBehavior::Transient
                );

                let content_view = {
                    let view = NSView::alloc(mtm);
                    NSView::initWithFrame(view, window_frame)
                };

                content_view.setWantsLayer(true);
                content_view.setAutoresizesSubviews(false);

                // Create and configure the layer for the border
                let layer = {
                    let layer = CALayer::new();
                    layer.setFrame(ns_rect);
                    layer.setActions(Some(&NSDictionary::new()));

                    unsafe {
                        // transparent
                        let clear = CGColor::new_generic_rgb(0.0, 0.0, 0.0, 0.0);
                        let clear_ptr = clear.deref() as *const _;
                        let _: () = msg_send![&layer, setBackgroundColor: clear_ptr];

                        let red = CGColor::new_generic_rgb(1.0, 0.0, 0.0, 1.0);
                        let red_ptr = red.deref() as *const _;
                        let _: () = msg_send![&layer, setBorderColor: red_ptr];
                        let _: () = msg_send![&layer, setBorderWidth: BORDER_WIDTH.load(Ordering::Relaxed) as f64];

                        let corner_radius: f64 = BORDER_RADIUS.load(Ordering::Relaxed) as f64;
                        let _: () = msg_send![&layer, setCornerRadius: corner_radius];

                        layer
                    }
                };

                content_view.setLayer(Some(&layer));

                window.setContentView(Some(&content_view));
                window.setMovableByWindowBackground(false);
                // Deliberately left hidden. set_visible shows it, and every path that
                // creates a border calls update() straight after, which does exactly that.
                //
                // Being born visible cost the focus flash whenever a border was created
                // rather than re-shown -- switching workspace destroys the old borders and
                // builds new ones, so the window was already on screen by the time anything
                // asked to show it, there was no transition to notice, and the flash never
                // fired. Starting hidden makes creation and re-showing look the same.
                //
                // (Whatever shows it must be orderFront, never makeKeyAndOrderFront: a
                // border is decoration, it ignores mouse events and has nothing to type
                // into. Making it the key window took keyboard focus away from whatever the
                // user was working in.)

                // What used to be here was an orderWindow_relativeTo call meant to keep
                // the border below the window in front of it. It never did anything:
                // AppKit window numbers are only meaningful within one process and the
                // target belongs to another application, so there was nothing to order
                // against (it also cast a u32 window id through i16, mangling any id
                // above 32767).
                //
                // Doing it properly is not possible from here. A border wants to sit
                // above its own window and below whatever covers it, and macOS offers no
                // way to reach that position across processes: SkyLight's SLSOrderWindow
                // reports success and leaves the border at the back. So the border stays
                // at floating level, above everything, and windows that come up in front
                // of it are drawn under it. Measured, not assumed -- see the SLSORDER
                // instrumentation that was here.
                let _ = target_window_id;
                if let Err(error) = tx.send(NsWindow { window, layer }) {
                    tracing::error!("could not send NSWindow created for border: {error}")
                }

            })
        });

        rx.recv()
            .ok()
            .ok_or_eyre("could not create a border NSWindow")
    }

    /// Show or hide the border.
    ///
    /// Ordering windows is AppKit work and has to happen on the main thread, which is
    /// not where the border manager runs.
    ///
    /// Returns whether this actually changed anything, so the caller can tell a border
    /// appearing (its window has just been focused) from one that was already showing.
    pub fn set_visible(&self, visible: bool) -> bool {
        let was_visible = self.window.isVisible();

        if was_visible == visible {
            return false;
        }

        let window_ptr = Retained::as_ptr(&self.window) as usize;

        DispatchQueue::main().exec_async(move || {
            autoreleasepool(|_| unsafe {
                let window = window_ptr as *const NSWindow;

                if visible {
                    (*window).orderFront(None);
                } else {
                    (*window).orderOut(None);
                }
            });
        });

        true
    }

    /// Flare the border briefly, then settle to its normal width.
    ///
    /// Used when a window takes focus, as a moment of movement to catch the eye where
    /// the attention is meant to go.
    ///
    /// Core Animation interpolates this itself: one animation is handed over and the
    /// compositor runs it. No timer thread stepping a value sixty times a second, and
    /// nothing left running once it ends -- which matters because this fires on every
    /// focus change.
    pub fn flash(&self, settled_width: f64, style: FlashStyle, _colour: Rgb) {
        if matches!(style, FlashStyle::None) {
            return;
        }

        let layer_ptr = Retained::as_ptr(&self.layer) as usize;

        DispatchQueue::main().exec_async(move || {
            autoreleasepool(|_| unsafe {
                let layer = &*(layer_ptr as *const CALayer);

                // Each style animates a different property of the border, so they are
                // built separately and handed to Core Animation the same way.
                let animation = match style {
                    FlashStyle::None => return,

                    // Flares wide and settles. Reads as a strike.
                    FlashStyle::Width => {
                        let factor = FLASH_FACTOR_X10.load(Ordering::Relaxed) as f64 / 10.0;
                        let a = basic_animation("borderWidth");
                        a.setFromValue(Some(&NSNumber::new_f64(settled_width * factor)));
                        a.setToValue(Some(&NSNumber::new_f64(settled_width)));
                        a
                    }




                };

                let duration = FLASH_DURATION_MS.load(Ordering::Relaxed) as f64 / 1000.0;
                let _: () = msg_send![&animation, setDuration: duration];

                // The curve decides whether this reads as a strike that decays (easeOut),
                // a swell (easeIn), something that eases at both ends (easeInEaseOut), or
                // a flat mechanical wipe (linear).
                let timing =
                    CAMediaTimingFunction::functionWithName(&NSString::from_str(&flash_easing()));
                let _: () = msg_send![&animation, setTimingFunction: &*timing];

                layer.addAnimation_forKey(&animation, Some(&NSString::from_str("focus-flash")));
            });
        });
    }

    pub fn set_border_color(&self, rgb: Rgb) {
        unsafe {
            // this is ass
            let color = CGColor::new_generic_rgb(
                rgb.r as f64 / 255.0,
                rgb.g as f64 / 255.0,
                rgb.b as f64 / 255.0,
                1.0,
            );
            let color_ptr = color.deref() as *const _;
            let _: () = msg_send![&self.layer, setBorderColor: color_ptr];
        }
    }

    /// Set the corner radius, so a border can match the window it frames.
    pub fn set_corner_radius(&self, radius: f64) {
        unsafe {
            let _: () = msg_send![&self.layer, setCornerRadius: radius];
        }
    }

    pub fn set_border_width(&self, width: f64) {
        unsafe {
            let _: () = msg_send![&self.layer, setBorderWidth: width];
        }
    }
}
