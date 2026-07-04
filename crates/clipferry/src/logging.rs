//! Minimal stderr logger (§9): no timestamps (journald adds them), no colors,
//! no regex filtering — `log` facade + ~20 lines, not `tracing`/`env_logger`.

#![allow(clippy::print_stderr)] // writing stderr is this module's entire job

use log::{LevelFilter, Log, Metadata, Record};

struct StderrLogger;

static LOGGER: StderrLogger = StderrLogger;

impl Log for StderrLogger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.level() <= log::max_level()
    }

    fn log(&self, record: &Record<'_>) {
        if self.enabled(record.metadata()) {
            eprintln!("{:5} {}", record.level(), record.args());
        }
    }

    fn flush(&self) {}
}

pub fn init(level: LevelFilter) {
    // Err only if a logger is already set — harmless in that case.
    let _ = log::set_logger(&LOGGER);
    log::set_max_level(level);
}
