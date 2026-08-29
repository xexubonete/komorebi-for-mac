//! Thread quality of service.
//!
//! Every komorebi thread was running in the default class, which is what macOS gives to
//! work nobody is waiting for. The threads that place windows are the opposite of that:
//! someone has just pressed a key and is looking at the screen.
//!
//! It matters twice over. The obvious way is scheduling -- a user-interactive thread is
//! preferred when the machine is busy, which is exactly when a workspace change feels
//! slow. The less obvious way is that macOS propagates quality of service across
//! synchronous IPC: an Accessibility call is a mach message, and the thread that services
//! it inside the other application inherits the priority of the thread that sent it. The
//! measurements say one slow application defines the cost of a whole workspace, so how
//! promptly that application gets scheduled to answer is not a detail.
//!
//! Raising this is not a way to take the machine over: the class says what the work is
//! for, and macOS still arbitrates. Background work is left where it belongs.

/// The values of `qos_class_t` from `<sys/qos.h>`.
#[derive(Copy, Clone)]
#[repr(u32)]
pub enum QosClass {
    /// Work the user is waiting to see the result of, right now.
    UserInteractive = 0x21,
    /// Work the user asked for and is waiting on, but not frame by frame.
    UserInitiated = 0x19,
    /// Work that should not compete with anything the user is doing.
    Utility = 0x11,
}

unsafe extern "C" {
    fn pthread_set_qos_class_self_np(qos_class: u32, relative_priority: i32) -> i32;
}

/// Put the calling thread in a quality of service class. Does nothing if macOS refuses,
/// which it does for threads whose class has been fixed by whoever created them.
pub fn set_for_current_thread(class: QosClass) {
    let result = unsafe { pthread_set_qos_class_self_np(class as u32, 0) };

    if result != 0 {
        tracing::debug!("could not set thread quality of service: error {result}");
    }
}
