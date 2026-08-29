// Private SkyLight/CoreGraphics APIs
//
// Read operations (CGS*) are used for querying window server state.
// Write operations (SLS*) are used for animation screen update batching.
unsafe extern "C" {
    // Read operations - these APIs are undocumented but stable - their usage should not disrupt the window manager.
    pub fn CGSMainConnectionID() -> i32;
    pub fn CGSGetActiveSpace(cid: i32) -> u64;

    // Write operations for animation - these APIs are undocumented and I don't know how stable they are
    // SLSDisableUpdate freezes screen compositing - all window changes are batched
    pub fn SLSDisableUpdate(cid: i32) -> i32;
    // SLSReenableUpdate resumes compositing - all batched changes appear at once
    pub fn SLSReenableUpdate(cid: i32) -> i32;

    // Compositor-level window move — bypasses Accessibility API IPC,
    // operates directly on the window server for sub-frame latency.
    // point is (x, y) in CG screen coordinates (origin top-left).
    pub fn SLSMoveWindow(cid: i32, wid: u32, point: *const objc2_core_foundation::CGPoint) -> i32;
}

use parking_lot::Mutex;

/// How many live scopes are asking for the screen to hold still.
static FROZEN_DEPTH: Mutex<usize> = Mutex::new(0);

/// Hold the screen still until this is dropped.
///
/// The window server composites every window move as it happens, so a workspace change --
/// several windows coming back from off-screen, several more being parked there -- is
/// drawn as a sequence of separate frames. What that looks like is windows arriving one
/// at a time and the previous workspace leaving piece by piece.
///
/// Freezing compositing batches the whole thing into a single frame. This already existed
/// around the layout pass, which is why the incoming windows arrive together; the stages
/// either side of it -- focusing, and hiding the workspace being left -- were outside it
/// and still drew themselves one window at a time.
///
/// Nesting is safe and is the point: an outer scope covering a whole workspace change
/// makes the inner one around the layout a no-op, so the screen thaws once, at the end.
///
/// If komorebi were to die holding this, macOS re-enables compositing by itself after
/// about a second. The screen cannot stay frozen.
pub struct ScreenHeldStill {
    connection_id: i32,
}

pub fn hold_screen_still() -> ScreenHeldStill {
    let connection_id = unsafe { CGSMainConnectionID() };
    let mut depth = FROZEN_DEPTH.lock();

    if *depth == 0 {
        unsafe { SLSDisableUpdate(connection_id) };
    }

    *depth += 1;

    ScreenHeldStill { connection_id }
}

impl Drop for ScreenHeldStill {
    fn drop(&mut self) {
        let mut depth = FROZEN_DEPTH.lock();
        *depth = depth.saturating_sub(1);

        if *depth == 0 {
            unsafe { SLSReenableUpdate(self.connection_id) };
        }
    }
}
