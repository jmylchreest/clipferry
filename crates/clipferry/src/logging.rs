//! Minimal stderr logger (§9): logfmt-style k=v messages, no timestamps
//! (journald adds them), no regex filtering — `log` facade + a few dozen
//! lines, not `tracing`/`env_logger`.
//!
//! Under systemd (detected via `$JOURNAL_STREAM`) each line carries an
//! sd-daemon `<N>` priority prefix so journald records real priorities and
//! `journalctl -p warning` filters work. On a terminal the textual level
//! is printed instead.

#![allow(clippy::print_stderr)] // writing stderr is this module's entire job

use std::sync::atomic::{AtomicBool, Ordering};

use log::{Level, LevelFilter, Log, Metadata, Record};

static JOURNAL: AtomicBool = AtomicBool::new(false);

struct StderrLogger;

static LOGGER: StderrLogger = StderrLogger;

impl Log for StderrLogger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.level() <= log::max_level()
    }

    fn log(&self, record: &Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let level = match record.level() {
            Level::Error => "error",
            Level::Warn => "warn",
            Level::Info => "info",
            Level::Debug => "debug",
            Level::Trace => "trace",
        };
        if JOURNAL.load(Ordering::Relaxed) {
            // sd-daemon(3) priority prefix (journald metadata) + level= as a
            // logfmt field so `-o cat` output stays self-contained.
            let priority = match record.level() {
                Level::Error => 3,
                Level::Warn => 4,
                Level::Info => 6,
                Level::Debug | Level::Trace => 7,
            };
            eprintln!("<{priority}>level={level} {}", record.args());
        } else {
            eprintln!("level={level} {}", record.args());
        }
    }

    fn flush(&self) {}
}

/// systemd sets `$JOURNAL_STREAM` to `dev:inode` of the journal socket; it
/// is only authoritative if our stderr actually *is* that socket — the env
/// var survives into unrelated children, so validate per sd-daemon(3).
fn stderr_is_journal() -> bool {
    let Some(value) = std::env::var_os("JOURNAL_STREAM") else {
        return false;
    };
    let Some((dev, ino)) = value.to_str().and_then(|v| v.split_once(':')) else {
        return false;
    };
    let (Ok(dev), Ok(ino)) = (dev.parse::<u64>(), ino.parse::<u64>()) else {
        return false;
    };
    rustix::fs::fstat(std::io::stderr()).is_ok_and(|st| st.st_dev == dev && st.st_ino == ino)
}

pub fn init(level: LevelFilter) {
    JOURNAL.store(stderr_is_journal(), Ordering::Relaxed);
    // Err only if a logger is already set — harmless in that case.
    let _ = log::set_logger(&LOGGER);
    log::set_max_level(level);
}
