//! Per-application minimum widths, learned once and remembered on disk.
//!
//! Some applications refuse to shrink below a width of their own choosing --
//! measured here: Música 980, WhatsApp 970. Asking for less silently fails: the
//! Accessibility call succeeds, the window keeps its size, and it ends up
//! overlapping whatever sits next to it in the layout.
//!
//! What is stored is the *cause* (this app will not go below N points), not the
//! situations it breaks in. One number per application answers the question for
//! any screen and any number of windows, so nothing has to be relearned when a
//! different monitor is plugged in.
//!
//! Entries are keyed by application name rather than window id, because a window
//! gets a fresh id every time the app is reopened.

use crate::DATA_DIR;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::LazyLock;

static MIN_WIDTHS: LazyLock<Mutex<HashMap<String, i32>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn min_widths_path() -> PathBuf {
    DATA_DIR.join("komorebi.min-widths.json")
}

/// Read the stored widths, seeding the ones already measured so that even a first
/// ever run does not have to discover them by getting them wrong once.
pub fn load() {
    let mut known: HashMap<String, i32> = HashMap::from([
        (String::from("Música"), 980),
        (String::from("Music"), 980),
        (String::from("WhatsApp"), 970),
    ]);

    if let Ok(contents) = std::fs::read_to_string(min_widths_path())
        && let Ok(stored) = serde_json::from_str::<HashMap<String, i32>>(&contents)
    {
        // What was learned on this machine wins over the seeded defaults: an app
        // update can change its minimum, and the observed value is the real one.
        known.extend(stored);
    }

    tracing::info!("loaded minimum widths for {} applications", known.len());
    *MIN_WIDTHS.lock() = known;
}

fn save(known: &HashMap<String, i32>) {
    let json = match serde_json::to_string_pretty(known) {
        Ok(json) => json,
        Err(error) => {
            tracing::warn!("could not serialize minimum widths: {error}");
            return;
        }
    };

    if let Err(error) = std::fs::write(min_widths_path(), json) {
        tracing::warn!("could not write minimum widths: {error}");
    }
}

/// The minimum width known for an application, if any.
///
/// A stored zero means "leave this app alone": an escape hatch that can be edited
/// into the file by hand when the automatic handling gets an app wrong.
pub fn get(application: &str) -> Option<i32> {
    match MIN_WIDTHS.lock().get(application).copied() {
        Some(0) | None => None,
        Some(width) => Some(width),
    }
}

/// Record a width an application refused to go below. Only ever grows: a window
/// can end up wider than its minimum for other reasons, so the largest refusal
/// observed is the safest estimate of where the floor actually is.
pub fn record(application: &str, width: i32) {
    let mut known = MIN_WIDTHS.lock();

    match known.get(application) {
        // Zero was set deliberately to opt this app out; don't undo that by
        // relearning the width the next time it refuses to shrink.
        Some(0) => return,
        Some(existing) if *existing >= width => return,
        _ => {}
    }

    tracing::info!("learned minimum width for {application}: {width}");
    known.insert(application.to_string(), width);
    save(&known);
}
