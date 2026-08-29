use crate::window_manager_event::WindowManagerEvent;
use crossbeam_channel::Receiver;
use crossbeam_channel::Sender;
use std::sync::OnceLock;

static CHANNEL: OnceLock<(Sender<WindowManagerEvent>, Receiver<WindowManagerEvent>)> =
    OnceLock::new();

/// Accessibility notifications arrive in bursts (measured: up to 40 in 50ms), but
/// the consumer drains them one at a time while holding the window manager lock
/// (measured: ~49/s median). The channel fills in ~25ms of a burst and `try_send`
/// then discards whatever comes next -- 7.3% of all notifications on a normal
/// session.
///
/// Dropping is not harmless: `try_send` cannot pick what it discards, and while
/// MoveEnd/ResizeEnd are idempotent, Destroy and FocusChange are not. A dropped
/// Destroy leaves a phantom window in the layout; a dropped FocusChange desyncs
/// focus and the border.
///
/// 256, after measuring. Raising it was held back for a while because a longer queue
/// trades lost events for stale ones -- the consumer acting on windows that have since
/// gone away, which shows up as AXError::InvalidUIElement. Two things settled it:
/// skipping windows already in position cut the burst peak from 40 events per 50ms to
/// 11, so the queue no longer runs deep enough for staleness to bite; and dropping
/// Destroy events was doing visible damage, leaving borders behind for windows that had
/// closed. 256 leaves roughly 5x headroom over the worst one-second peak observed.
///
/// The producer is an Accessibility callback on the main run loop, so it must
/// never block -- hence a bounded channel and `try_send`.
const EVENT_CHANNEL_CAPACITY: usize = 256;

fn channel() -> &'static (Sender<WindowManagerEvent>, Receiver<WindowManagerEvent>) {
    CHANNEL.get_or_init(|| crossbeam_channel::bounded(EVENT_CHANNEL_CAPACITY))
}

fn event_tx() -> Sender<WindowManagerEvent> {
    channel().0.clone()
}

pub fn event_rx() -> Receiver<WindowManagerEvent> {
    channel().1.clone()
}

/// Whether losing this event would leave komorebi believing something untrue.
///
/// MoveEnd and ResizeEnd only ever say "this window's geometry settled"; the next one
/// supersedes the last, and the position is read from the window when handled, so
/// dropping one costs nothing. Everything else reports a change of state that nothing
/// will repeat -- a window closed, focus moved -- and losing it leaves komorebi acting
/// on a world that no longer exists.
fn is_replaceable(event: &WindowManagerEvent) -> bool {
    matches!(
        event,
        WindowManagerEvent::MoveEnd(_, _, _) | WindowManagerEvent::ResizeEnd(_, _, _)
    )
}

/// How long to keep trying to deliver an event that cannot be replaced.
///
/// The producer is an Accessibility callback on the main run loop, so this must stay
/// short enough not to be felt: a few milliseconds is long enough for the consumer to
/// take one item off a full queue, and short enough to be invisible.
const UNREPLACEABLE_SEND_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(5);

pub fn send_notification(notification: WindowManagerEvent) {
    // Geometry settling: the next one supersedes this, so a full queue can simply
    // swallow it.
    if is_replaceable(&notification) {
        if let Err(error) = event_tx().try_send(notification) {
            tracing::warn!(
                "channel is full ({}/{} queued); dropping notification: {}",
                event_tx().len(),
                EVENT_CHANNEL_CAPACITY,
                error.into_inner()
            );
        }

        return;
    }

    // Everything else says something happened that nothing will say again. Wait briefly
    // for room rather than discarding it: dropped Destroy events were leaving borders on
    // screen for windows that had already closed.
    if let Err(error) = event_tx().send_timeout(notification, UNREPLACEABLE_SEND_TIMEOUT) {
        tracing::warn!(
            "channel still full after {}ms; dropping unreplaceable notification: {}",
            UNREPLACEABLE_SEND_TIMEOUT.as_millis(),
            error.into_inner()
        );
    }
}
