//! Per-application minimum sizes, learned once and remembered on disk.
//!
//! Some applications refuse to shrink below a size of their own choosing. Asking
//! for less silently fails: the Accessibility call succeeds, the window keeps the
//! size it had, and it ends up overlapping whatever sits next to it in the layout.
//!
//! Both dimensions are refused, and independently. Measured here: WhatsApp and
//! Música will not go below 600 points tall, and hold widths near 1000; Mail
//! refuses widths in the 750s. Which one bites depends on the grid: dense columns
//! hit the width, dense rows hit the height.
//!
//! Refusing is not cheap either. The application relayouts its whole interface
//! before declining, and because the window never lands where it was asked to go,
//! the "already in place" check never matches it and every single layout pass pays
//! that cost again. Asking for something it will accept ends both problems.
//!
//! What is stored is the *cause* (this app will not go below N points), not the
//! situations it breaks in. One pair of numbers per application answers the
//! question for any screen and any number of windows, so nothing has to be
//! relearned when a different monitor is plugged in.
//!
//! Entries are keyed by application name rather than window id, because a window
//! gets a fresh id every time the app is reopened.

use crate::DATA_DIR;
use parking_lot::Mutex;
use serde::Deserialize;
use serde::Serialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::LazyLock;

/// The smallest an application has ever been seen to accept. A zero in either
/// dimension means "leave this app alone in that dimension": an escape hatch that
/// can be edited into the file by hand when the automatic handling gets one wrong.
#[derive(Copy, Clone, Default, Serialize, Deserialize)]
pub struct MinSize {
    #[serde(default)]
    pub width: i32,
    #[serde(default)]
    pub height: i32,
}

static MIN_SIZES: LazyLock<Mutex<HashMap<String, MinSize>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn min_sizes_path() -> PathBuf {
    DATA_DIR.join("komorebi.min-sizes.json")
}

/// Read the stored sizes, seeding the ones already measured so that even a first
/// ever run does not have to discover them by getting them wrong once.
pub fn load() {
    let mut known: HashMap<String, MinSize> = HashMap::from([
        (
            String::from("Música"),
            MinSize {
                width: 980,
                height: 600,
            },
        ),
        (
            String::from("Music"),
            MinSize {
                width: 980,
                height: 600,
            },
        ),
        (
            String::from("WhatsApp"),
            MinSize {
                width: 970,
                height: 600,
            },
        ),
    ]);

    if let Ok(contents) = std::fs::read_to_string(min_sizes_path())
        && let Ok(stored) = serde_json::from_str::<HashMap<String, MinSize>>(&contents)
    {
        // What was learned on this machine wins over the seeded defaults: an app
        // update can change its minimum, and the observed value is the real one.
        known.extend(stored);
    }

    tracing::info!("loaded minimum sizes for {} applications", known.len());
    *MIN_SIZES.lock() = known;
}

fn save(known: &HashMap<String, MinSize>) {
    let json = match serde_json::to_string_pretty(known) {
        Ok(json) => json,
        Err(error) => {
            tracing::warn!("could not serialize minimum sizes: {error}");
            return;
        }
    };

    if let Err(error) = std::fs::write(min_sizes_path(), json) {
        tracing::warn!("could not write minimum sizes: {error}");
    }
}

/// The minimum width known for an application, if any.
pub fn get(application: &str) -> Option<i32> {
    match MIN_SIZES.lock().get(application).map(|size| size.width) {
        Some(0) | None => None,
        width => width,
    }
}

/// The minimum height known for an application, if any.
pub fn get_height(application: &str) -> Option<i32> {
    match MIN_SIZES.lock().get(application).map(|size| size.height) {
        Some(0) | None => None,
        height => height,
    }
}

/// Record a size an application refused to go below. Only ever grows: a window can
/// end up larger than its minimum for other reasons, so the largest refusal observed
/// is the safest estimate of where the floor actually is.
///
/// A dimension passed as zero is not being reported on and is left as it was.
pub fn record(application: &str, width: i32, height: i32) {
    let mut known = MIN_SIZES.lock();
    let existing = known.get(application).copied().unwrap_or_default();
    let mut updated = existing;

    // Zero was set deliberately to opt this app out of a dimension; don't undo that
    // by relearning it the next time the app refuses to shrink.
    if width > existing.width && !(existing.width == 0 && known.contains_key(application)) {
        updated.width = width;
    }

    if height > existing.height && !(existing.height == 0 && known.contains_key(application)) {
        updated.height = height;
    }

    if updated.width == existing.width && updated.height == existing.height {
        return;
    }

    tracing::info!(
        "learned minimum size for {application}: {}x{}",
        updated.width,
        updated.height
    );

    known.insert(application.to_string(), updated);
    save(&known);
}

/// The smallest size each application has actually been seen to accept.
static ACCEPTED: LazyLock<Mutex<HashMap<String, MinSize>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Whether asking where a window ended up would teach anything.
///
/// After placing a window komorebi reads its geometry back, to notice when an
/// application quietly ignored the size it was given. That read is a second round trip
/// to a process that has just finished making komorebi wait -- measured at 46ms on
/// average for WhatsApp, and 237ms at worst, a quarter of what its placements cost.
///
/// It is only worth paying while there is something to find out. An application that has
/// already accepted a size at least this small has nothing left to refuse here, so the
/// question has a known answer and does not need asking.
pub fn worth_verifying(application: &str, width: i32, height: i32) -> bool {
    match ACCEPTED.lock().get(application) {
        Some(smallest) => width < smallest.width || height < smallest.height,
        None => true,
    }
}

/// Note a size an application took without complaint.
pub fn note_accepted(application: &str, width: i32, height: i32) {
    let mut accepted = ACCEPTED.lock();
    let smallest = accepted.entry(application.to_string()).or_insert(MinSize {
        width: i32::MAX,
        height: i32::MAX,
    });

    smallest.width = smallest.width.min(width);
    smallest.height = smallest.height.min(height);
}

/// An application has refused something: what it used to accept is no longer a promise.
pub fn forget_accepted(application: &str) {
    ACCEPTED.lock().remove(application);
}
