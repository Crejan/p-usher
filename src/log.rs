//! Minimal stderr logging. Under systemd (`JOURNAL_STREAM` set) lines carry a
//! `<N>` syslog priority prefix so the journal records the right level.

use std::fmt;
use std::sync::OnceLock;

pub const ERROR: u8 = 3;
pub const WARN: u8 = 4;
pub const INFO: u8 = 6;

fn journal() -> bool {
    static JOURNAL: OnceLock<bool> = OnceLock::new();
    *JOURNAL.get_or_init(|| std::env::var_os("JOURNAL_STREAM").is_some())
}

pub fn emit(level: u8, args: fmt::Arguments) {
    if journal() {
        eprintln!("<{level}>{args}");
    } else {
        let label = match level {
            ERROR => "error",
            WARN => "warning",
            _ => "info",
        };
        eprintln!("p-usher: {label}: {args}");
    }
}

#[macro_export]
macro_rules! log_error {
    ($($arg:tt)*) => { $crate::log::emit($crate::log::ERROR, format_args!($($arg)*)) };
}

#[macro_export]
macro_rules! log_warn {
    ($($arg:tt)*) => { $crate::log::emit($crate::log::WARN, format_args!($($arg)*)) };
}

#[macro_export]
macro_rules! log_info {
    ($($arg:tt)*) => { $crate::log::emit($crate::log::INFO, format_args!($($arg)*)) };
}
