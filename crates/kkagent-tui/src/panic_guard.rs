//! Panic-time terminal restore.
//!
//! `TuiApp::run` restores the terminal on every normal return path, but a
//! panic unwinds past that cleanup: raw mode stays enabled (the shell stops
//! echoing), mouse capture and bracketed paste stay on (clicks and pastes
//! inject garbage), and the alternate screen freezes on the last frame. This
//! module wraps the process panic hook: when a panic happens on the thread
//! currently driving the TUI event loop while the TUI owns the terminal, the
//! terminal is restored *before* the chained hooks run, so panic output lands
//! on a normal screen and the shell stays usable.
//!
//! Thread gating matters: background jobs (async tasks, output pumps) can
//! panic without taking the display down with them — those panics are
//! recovered by the task machinery and the TUI keeps running. Only the
//! event-loop thread's panics are fatal for the display.

use std::io;
use std::panic;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::thread::ThreadId;

use crossterm::cursor::Show;
use crossterm::event::{DisableBracketedPaste, DisableMouseCapture};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, LeaveAlternateScreen};

static TUI_ACTIVE: AtomicBool = AtomicBool::new(false);
static OWNER_THREAD: Mutex<Option<ThreadId>> = Mutex::new(None);

/// Whether the panic hook should restore the terminal.
fn should_restore(active: bool, is_tui_thread: bool) -> bool {
    active && is_tui_thread
}

pub fn set_active(active: bool) {
    TUI_ACTIVE.store(active, Ordering::SeqCst);
}

/// Record the thread currently driving the event loop. Called at install time
/// and from every frame: the multi-thread tokio runtime may migrate the TUI
/// task between worker threads at await points, so the freshest rendering
/// thread is the authoritative owner.
pub fn note_owner_thread() {
    if let Ok(mut owner) = OWNER_THREAD.lock() {
        *owner = Some(std::thread::current().id());
    }
}

fn is_owner_thread() -> bool {
    OWNER_THREAD
        .lock()
        .ok()
        .and_then(|owner| *owner)
        .is_some_and(|owner| owner == std::thread::current().id())
}

/// Wrap the current panic hook with terminal restoration. Chained hooks
/// (diagnostics JSON + the default panic message) still run afterwards.
pub fn install() {
    note_owner_thread();
    let previous = panic::take_hook();
    panic::set_hook(Box::new(move |panic_info| {
        if should_restore(TUI_ACTIVE.load(Ordering::SeqCst), is_owner_thread()) {
            restore_terminal();
            // One-shot: a second panic while unwinding must not restore twice.
            TUI_ACTIVE.store(false, Ordering::SeqCst);
        }
        previous(panic_info);
    }));
}

/// Mirror of `TuiApp::run`'s teardown sequence, same order (app.rs). Every
/// step is best effort: a half-broken terminal must not abort the rest.
/// LeaveAlternateScreen / DisableMouseCapture are no-ops when the matching
/// mode was never enabled, so this is safe for `--no-alt-screen` and
/// `KKAGENT_MOUSE_MODE=off`.
fn restore_terminal() {
    let mut stdout = io::stdout();
    let _ = disable_raw_mode();
    let _ = execute!(
        stdout,
        DisableMouseCapture,
        DisableBracketedPaste,
        LeaveAlternateScreen,
        Show
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn restore_requires_active_and_owner_thread() {
        assert!(should_restore(true, true));
        assert!(!should_restore(true, false));
        assert!(!should_restore(false, true));
        assert!(!should_restore(false, false));
    }

    /// Exercises the installed hook end to end. TUI_ACTIVE doubles as the
    /// observable: the hook clears it exactly when it restored the terminal.
    /// All scenarios run in one test because the panic hook is process-global.
    #[test]
    fn hook_restores_only_for_owner_thread_while_active() {
        use std::sync::atomic::AtomicUsize;

        static PREVIOUS_CALLS: AtomicUsize = AtomicUsize::new(0);
        let original = panic::take_hook();
        panic::set_hook(Box::new(|_| {
            PREVIOUS_CALLS.fetch_add(1, Ordering::SeqCst);
        }));
        install();
        TUI_ACTIVE.store(false, Ordering::SeqCst);

        // Inactive: delegate to the previous hook, never touch the terminal.
        let _ = panic::catch_unwind(|| panic!("inactive"));
        assert_eq!(PREVIOUS_CALLS.load(Ordering::SeqCst), 1);
        assert!(!TUI_ACTIVE.load(Ordering::SeqCst));

        // Foreign thread while active: background-job panic, display must
        // survive — no restore (flag stays set), previous still runs.
        TUI_ACTIVE.store(true, Ordering::SeqCst);
        let done = Arc::new(AtomicBool::new(false));
        let seen = Arc::clone(&done);
        let _ = std::thread::spawn(move || {
            let _ = panic::catch_unwind(|| panic!("background"));
            seen.store(true, Ordering::SeqCst);
        })
        .join();
        assert!(done.load(Ordering::SeqCst));
        assert_eq!(PREVIOUS_CALLS.load(Ordering::SeqCst), 2);
        assert!(TUI_ACTIVE.load(Ordering::SeqCst));

        // Owner thread while active: restore runs exactly once (flag flips).
        let _ = panic::catch_unwind(|| panic!("tui thread"));
        assert_eq!(PREVIOUS_CALLS.load(Ordering::SeqCst), 3);
        assert!(!TUI_ACTIVE.load(Ordering::SeqCst));

        // Leave the process hook as we found it.
        drop(panic::take_hook());
        panic::set_hook(original);
    }
}
