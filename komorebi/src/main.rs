#![warn(clippy::all)]

use clap::Parser;
use clap::ValueEnum;
use color_eyre::eyre;
use color_eyre::eyre::OptionExt;
use komorebi::CoreFoundationRunLoop;
use komorebi::DATA_DIR;
use komorebi::HOME_DIR;
use komorebi::border_manager;
use komorebi::core::pathext::replace_env_in_path;
use komorebi::display_reconfiguration_listener::DisplayReconfigurationListener;
use komorebi::focus_follows_mouse;
use komorebi::input_event_listener::InputEventListener;
use komorebi::monitor_reconciliator;
use komorebi::notification_center_listener::NotificationCenterListener;
use komorebi::process_command::listen_for_commands;
use komorebi::process_event::listen_for_events;
use komorebi::reaper;
use komorebi::static_config::StaticConfig;
use komorebi::theme_manager;
use komorebi::window_manager::WindowManager;
use komorebi::window_manager_event_listener;
use komorebi::workspace_reconciliator;
use objc2::MainThreadMarker;
use objc2::rc::autoreleasepool;
use objc2_app_kit::NSApplication;
use objc2_app_kit::NSEventMask;
use objc2_application_services::AXIsProcessTrusted;
use objc2_application_services::AXIsProcessTrustedWithOptions;
use objc2_application_services::kAXTrustedCheckOptionPrompt;
use objc2_core_foundation::CFBoolean;
use objc2_core_foundation::CFDictionary;
use objc2_core_foundation::CFRunLoop;
use objc2_core_foundation::CFString;
use objc2_core_foundation::kCFRunLoopDefaultMode;
use objc2_core_graphics::CGPreflightScreenCaptureAccess;
use objc2_core_graphics::CGRequestScreenCaptureAccess;
use objc2_foundation::NSDate;
use objc2_foundation::NSDefaultRunLoopMode;
use parking_lot::Mutex;
use serde::Deserialize;
use std::ffi::c_void;
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use sysinfo::Process;
use sysinfo::ProcessesToUpdate;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;

/// Which of the two macOS permissions komorebi needs are granted right now.
///
/// The check lives in this binary on purpose. macOS grants permissions per code
/// signature, so a shell script asking on komorebi's behalf would only ever learn
/// whether *the terminal* is trusted. Only komorebi can answer for komorebi -- which
/// is why `--check-permissions` exists for the installer to call.
struct Permissions {
    screen_recording: bool,
    accessibility: bool,
}

impl Permissions {
    fn check() -> Self {
        Self {
            screen_recording: CGPreflightScreenCaptureAccess(),
            accessibility: unsafe { AXIsProcessTrusted() },
        }
    }

    fn all_granted(&self) -> bool {
        self.screen_recording && self.accessibility
    }
}

/// Put up the system dialog for whatever is missing.
///
/// Prompting is also what gets komorebi *listed* in System Settings: until a binary
/// asks, there is no row to tick, and adding a bare executable by hand is awkward.
/// So the installer prompts first and only then sends the user to the panel.
fn prompt_for_missing(permissions: &Permissions) {
    if !permissions.screen_recording {
        // Shows the system dialog and returns whether it ended up granted.
        CGRequestScreenCaptureAccess();
    }

    if !permissions.accessibility {
        // Prompting here is asynchronous and does not change the return value: the
        // dialog goes up and the answer is still whatever it was a moment ago. The
        // caller has to re-check later, which is exactly what the installer loop does.
        let key: *const c_void =
            unsafe { kAXTrustedCheckOptionPrompt } as *const CFString as *const c_void;
        let value = CFBoolean::new(true);
        let value_ptr: *const c_void = (&raw const *value).cast::<c_void>();

        let mut keys = [key];
        let mut values = [value_ptr];

        let options = unsafe {
            CFDictionary::new(
                None,
                keys.as_mut_ptr(),
                values.as_mut_ptr(),
                1,
                std::ptr::null(),
                std::ptr::null(),
            )
        };

        if let Some(options) = options {
            unsafe { AXIsProcessTrustedWithOptions(Some(&options)) };
        }
    }
}

/// Print the state of both permissions and exit 0 only if both are granted.
///
/// This is the installer's eyes: a script can open a settings panel, but it cannot
/// tell whether anyone acted on it. Exit code 1 is what lets it keep asking.
fn permissions_report(request: bool) -> ! {
    if request {
        prompt_for_missing(&Permissions::check());
    }

    // Re-read after prompting: the Accessibility dialog does not update the answer it
    // returns, so the only honest value is the one read fresh.
    let permissions = Permissions::check();

    let mark = |granted: bool| if granted { "granted" } else { "MISSING" };
    println!(
        "accessibility: {}",
        mark(permissions.accessibility)
    );
    println!(
        "screen-recording: {}",
        mark(permissions.screen_recording)
    );

    std::process::exit(if permissions.all_granted() { 0 } else { 1 })
}

fn check_permissions() -> eyre::Result<()> {
    // When launched via LaunchAgent at login, the WindowServer may not be
    // fully ready yet. The permission APIs return false even if the user has
    // already granted the permission. Retry a few times before giving up.
    for attempt in 1..=10 {
        let permissions = Permissions::check();

        if permissions.all_granted() {
            return Ok(());
        }

        if attempt < 10 {
            tracing::info!(
                "waiting for permissions (screen={}, accessibility={}), attempt {attempt}/10",
                permissions.screen_recording,
                permissions.accessibility
            );
            std::thread::sleep(std::time::Duration::from_secs(2));
            continue;
        }
    }

    // Out of retries, so this is a genuinely missing permission rather than a
    // WindowServer that had not caught up. Ask for it: on a machine that has never
    // granted it there is nobody to tell.
    //
    // The dialogs only appear when a permission is actually missing, and macOS
    // remembers the answer as long as the binary keeps a stable code signature --
    // which is why it must be built with `kbuild`, never with a bare `cargo build`.
    prompt_for_missing(&Permissions::check());

    let permissions = Permissions::check();

    // Only Accessibility is fatal: without it komorebi cannot move or resize a single
    // window, so starting would leave a process that does nothing. Screen Recording
    // only costs window titles, and a tiling manager without titles still tiles --
    // refusing to start over it trades a degraded desktop for no desktop at all.
    if !permissions.screen_recording {
        tracing::warn!(
            "screen recording permission not granted - window titles are unavailable, \
             so any rule that matches on a title will not apply. Grant it in System \
             Settings -> Privacy & Security -> Screen Recording"
        );
    }

    if !permissions.accessibility {
        eyre::bail!(
            "komorebi cannot run without the Accessibility permission, which is what \
             lets it move and resize windows. Grant it in System Settings -> Privacy \
             & Security -> Accessibility, then start komorebi again."
        );
    }

    Ok(())
}

fn setup(log_level: LogLevel) -> eyre::Result<(WorkerGuard, WorkerGuard)> {
    if std::env::var("RUST_LIB_BACKTRACE").is_err() {
        unsafe {
            std::env::set_var("RUST_LIB_BACKTRACE", "1");
        }
    }

    color_eyre::install()?;

    if std::env::var("RUST_LOG").is_err() {
        unsafe {
            std::env::set_var(
                "RUST_LOG",
                match log_level {
                    LogLevel::Error => "komorebi=error",
                    LogLevel::Warn => "komorebi=warn",
                    LogLevel::Info => "komorebi=info",
                    LogLevel::Debug => "komorebi=debug",
                    LogLevel::Trace => "komorebi=trace",
                },
            );
        }
    }

    let appender = tracing_appender::rolling::daily(&*DATA_DIR, "komorebi_plaintext.log");
    let color_appender = tracing_appender::rolling::daily(&*DATA_DIR, "komorebi.log");
    let (non_blocking, guard) = tracing_appender::non_blocking(appender);
    let (color_non_blocking, color_guard) = tracing_appender::non_blocking(color_appender);

    tracing::subscriber::set_global_default(
        tracing_subscriber::fmt::Subscriber::builder()
            .with_env_filter(EnvFilter::from_default_env())
            .finish()
            .with(
                tracing_subscriber::fmt::Layer::default()
                    .with_writer(non_blocking)
                    .with_ansi(false),
            )
            .with(
                tracing_subscriber::fmt::Layer::default()
                    .with_writer(color_non_blocking)
                    .with_ansi(true),
            ),
    )?;

    // https://github.com/tokio-rs/tracing/blob/master/examples/examples/panic_hook.rs
    // Set a panic hook that records the panic as a `tracing` event at the
    // `ERROR` verbosity level.
    //
    // If we are currently in a span when the panic occurred, the logged event
    // will include the current span, allowing the context in which the panic
    // occurred to be recorded.
    std::panic::set_hook(Box::new(|panic| {
        // If the panic has a source location, record it as structured fields.
        panic.location().map_or_else(
            || {
                tracing::error!(message = %panic);
            },
            |location| {
                // On nightly Rust, where the `PanicInfo` type also exposes a
                // `message()` method returning just the message, we could record
                // just the message instead of the entire `fmt::Display`
                // implementation, avoiding the duplciated location
                tracing::error!(
                    message = %panic,
                    panic.file = location.file(),
                    panic.line = location.line(),
                    panic.column = location.column(),
                );
            },
        );
    }));

    Ok((guard, color_guard))
}

#[cfg(feature = "deadlock_detection")]
#[tracing::instrument]
fn detect_deadlocks() {
    // Create a background thread which checks for deadlocks every 10s
    std::thread::spawn(move || {
        loop {
            tracing::info!("running deadlock detector");
            std::thread::sleep(std::time::Duration::from_secs(5));
            let deadlocks = parking_lot::deadlock::check_deadlock();
            if deadlocks.is_empty() {
                continue;
            }

            tracing::error!("{} deadlocks detected", deadlocks.len());
            for (i, threads) in deadlocks.iter().enumerate() {
                tracing::error!("deadlock #{}", i);
                for t in threads {
                    tracing::error!("thread id: {:#?}", t.thread_id());
                    tracing::error!("{:#?}", t.backtrace());
                }
            }
        }
    });
}

#[derive(Default, Deserialize, ValueEnum, Clone)]
#[serde(rename_all = "snake_case")]
enum LogLevel {
    Error,
    Warn,
    #[default]
    Info,
    Debug,
    Trace,
}

#[derive(Parser)]
#[clap(author, about, version = version::LONG_VERSION)]
struct Opts {
    /// Path to a static configuration JSON file
    #[clap(short, long)]
    #[clap(value_parser = replace_env_in_path)]
    config: Option<PathBuf>,
    // /// Do not attempt to auto-apply a dumped state temp file from a previously running instance of komorebi
    // #[clap(long)]
    // clean_state: bool,
    /// Level of log output verbosity
    #[clap(long, value_enum, default_value_t=LogLevel::Info)]
    log_level: LogLevel,
    /// Report whether the macOS permissions komorebi needs are granted, then exit.
    /// Exits 0 when both are, 1 when either is missing, so a script can act on it.
    #[clap(long)]
    check_permissions: bool,
    /// Like --check-permissions, but first show the system dialog for whatever is
    /// missing -- which is also what gets komorebi listed in System Settings.
    #[clap(long)]
    request_permissions: bool,
}

fn main() -> eyre::Result<()> {
    let opts: Opts = Opts::parse();

    // Answer the installer before doing anything else: this must not start a daemon,
    // touch the log files, or trip the single-instance check below.
    if opts.check_permissions || opts.request_permissions {
        permissions_report(opts.request_permissions);
    }

    let (_guard, _color_guard) = setup(opts.log_level)?;

    // The main thread runs the CoreFoundation run loop: every Accessibility notification
    // arrives here, and every border window is created and destroyed here. Nothing in
    // komorebi is closer to what the user sees.
    komorebi::qos::set_for_current_thread(komorebi::qos::QosClass::UserInteractive);

    let mut system = sysinfo::System::new();
    system.refresh_processes(ProcessesToUpdate::All, true);

    let matched_procs: Vec<&Process> = system
        .processes_by_exact_name("komorebi".as_ref())
        .collect();

    if matched_procs.len() > 1 {
        tracing::error!(
            "komorebi is already running, please exit the existing process before starting a new one"
        );
        std::process::exit(1);
    }

    check_permissions()?;

    if !DATA_DIR.is_dir() {
        std::fs::create_dir_all(&*DATA_DIR)?;
    }

    // Minimum widths learned in previous runs, so an app that will not fit a narrow
    // column is routed elsewhere from the first layout rather than after overlapping
    // its neighbour once.
    komorebi::min_size::load();

    let mtm = MainThreadMarker::new().ok_or_eyre("failed to create main thread marker")?;
    // apparently this establishes the window server connection which is needed super early on tahoe
    let app = NSApplication::sharedApplication(mtm);
    // this lets us complete initialization without using a blocking method like run
    app.finishLaunching();

    let _notification_center_listener = NotificationCenterListener::init();
    let _display_reconfiguration_listener = DisplayReconfigurationListener::init();

    let run_loop = CFRunLoop::current().ok_or_eyre("couldn't get CFRunLoop::current")?;
    let _input_listener = InputEventListener::init(&run_loop);

    #[cfg(feature = "deadlock_detection")]
    detect_deadlocks();

    let static_config = opts.config.map_or_else(
        || {
            let komorebi_json = HOME_DIR.join("komorebi.json");
            if komorebi_json.is_file() {
                Option::from(komorebi_json)
            } else {
                None
            }
        },
        Option::from,
    );

    let wm = if let Some(config) = &static_config {
        tracing::info!(
            "creating window manager from static configuration file: {}",
            config.display()
        );

        Arc::new(Mutex::new(StaticConfig::preload(
            config,
            window_manager_event_listener::event_rx(),
            None,
            &run_loop,
        )?))
    } else {
        Arc::new(Mutex::new(WindowManager::new(
            &run_loop,
            window_manager_event_listener::event_rx(),
            None,
        )?))
    };

    wm.lock().init()?;

    if let Some(config) = &static_config {
        StaticConfig::postload(config, &wm)?;
    }

    wm.lock().retile_all(false)?;

    // After restoring the session, windows may be spread across several
    // workspaces. retile_all only tiles the focused one; here we hide (move
    // off-screen) the windows of the non-focused workspaces on each monitor.
    {
        let mut wm = wm.lock();
        let mouse_follows_focus = wm.mouse_follows_focus;
        for monitor in wm.monitors_mut() {
            monitor.load_focused_workspace(mouse_follows_focus)?;
        }
    }

    border_manager::listen_for_notifications(wm.clone(), CoreFoundationRunLoop(run_loop));
    theme_manager::listen_for_notifications();
    monitor_reconciliator::listen_for_notifications(wm.clone())?;
    reaper::listen_for_notifications(wm.clone());
    workspace_reconciliator::listen_for_notifications(wm.clone());
    focus_follows_mouse::listen(wm.clone());

    listen_for_commands(wm.clone());
    listen_for_events(wm.clone());

    let quit_ctrlc = Arc::new(AtomicBool::new(false));
    let quit_thread = quit_ctrlc.clone();

    std::thread::spawn(move || {
        let (ctrlc_sender, ctrlc_receiver) = mpsc::channel();
        ctrlc::set_handler(move || {
            ctrlc_sender
                .send(())
                .expect("could not send signal on ctrl-c channel");
        })
        .expect("could not set ctrl-c handler");

        ctrlc_receiver
            .recv()
            .expect("could not receive signal on ctrl-c channel");

        tracing::info!("ctrl-c signal received");
        quit_ctrlc.store(true, Ordering::Relaxed);
    });

    tracing::info!("starting CFRunLoop to receive observer notifications");

    loop {
        if quit_thread.load(Ordering::Relaxed) {
            tracing::info!("stopping CFRunLoop");
            break;
        }

        autoreleasepool(|_| {
            // process NSApplication events explicitly - this is what makes display
            // reconfiguration callbacks work on tahoe
            if let Some(event) = unsafe {
                app.nextEventMatchingMask_untilDate_inMode_dequeue(
                    NSEventMask::Any,
                    Some(&NSDate::dateWithTimeIntervalSinceNow(0.1)),
                    NSDefaultRunLoopMode,
                    true,
                )
            } {
                app.sendEvent(&event);
            }

            // this gets our observer notification callbacks firing
            unsafe { CFRunLoop::run_in_mode(kCFRunLoopDefaultMode, 0.1, false) };
        });
    }

    wm.lock().restore_all_windows(false)?;

    let sockets = komorebi::SUBSCRIPTION_SOCKETS.lock();
    for path in (*sockets).values() {
        if let Ok(stream) = UnixStream::connect(path) {
            stream.shutdown(Shutdown::Both)?;
        }
    }

    let socket = DATA_DIR.join("komorebi.sock");
    let _ = std::fs::remove_file(socket);

    std::process::exit(130);
}

#[cfg(test)]
mod tests {
    use super::Permissions;

    // The rest of the permission code is a thin wrapper over TCC: whether it reports
    // the truth is a question about macOS, not about this crate, and only a machine
    // with the permission actually revoked could answer it. What is worth pinning down
    // is the rule that decides komorebi's fate, because getting it backwards is what
    // turns a degraded desktop into no desktop at all.
    #[test]
    fn all_granted_requires_both_permissions() {
        let cases = [
            (true, true, true),
            (true, false, false),
            (false, true, false),
            (false, false, false),
        ];

        for (screen_recording, accessibility, expected) in cases {
            let permissions = Permissions {
                screen_recording,
                accessibility,
            };

            assert_eq!(
                permissions.all_granted(),
                expected,
                "screen_recording={screen_recording}, accessibility={accessibility}"
            );
        }
    }
}
