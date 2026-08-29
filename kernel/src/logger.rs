//! Structured kernel logging: level-gated, mirrored to serial and screen.
//!
//! Macros: `trace!`, `debug!`, `info!`, `warn!`, `error!` (exported
//! crate-wide via `#[macro_export]`).

use core::fmt::{self, Write};
use kairos_core::config::LogLevel;

use crate::{serial, vga};

/// Compiled minimum level (from `KAIROS_LOG_LEVEL`).
fn min_level() -> LogLevel {
    kairos_core::config::LOG_LEVEL
}

/// A tiny bounded on-stack writer so we format one line, then emit once.
struct LineBuf {
    buf: [u8; 512],
    len: usize,
}

impl LineBuf {
    const fn new() -> Self {
        Self { buf: [0; 512], len: 0 }
    }
}

impl Write for LineBuf {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let room = self.buf.len() - self.len;
        let take = s.len().min(room);
        self.buf[self.len..self.len + take].copy_from_slice(&s.as_bytes()[..take]);
        self.len += take;
        Ok(())
    }
}

/// Emit a log record if its level passes the compiled threshold.
pub fn log(level: LogLevel, args: fmt::Arguments<'_>) {
    if level > min_level() {
        return;
    }
    let tag = match level {
        LogLevel::Error => "ERROR",
        LogLevel::Warn => "WARN ",
        LogLevel::Info => "INFO ",
        LogLevel::Debug => "DEBUG",
        LogLevel::Trace => "TRACE",
    };

    let mut line = LineBuf::new();
    let _ = line.write_str("[");
    let _ = line.write_str(tag);
    let _ = line.write_str("] ");
    let _ = line.write_fmt(args);
    let text = unsafe { core::str::from_utf8_unchecked(&line.buf[..line.len]) };

    serial::write_line(text);
    if level == LogLevel::Error || level == LogLevel::Warn {
        vga::print(format_args!("[{tag}] {text}\n"));
    }
}

pub fn init() {
    log(LogLevel::Info, format_args!("logger: ready (level={:?})", min_level()));
}

// The level macros live below; init/test_echo call `log` directly to avoid
// relying on textual macro order.

#[macro_export]
macro_rules! trace {
    ($($arg:tt)*) => { $crate::logger::log($crate::LogLevel::Trace, format_args!($($arg)*)) };
}

#[macro_export]
macro_rules! debug {
    ($($arg:tt)*) => { $crate::logger::log($crate::LogLevel::Debug, format_args!($($arg)*)) };
}

#[macro_export]
macro_rules! info {
    ($($arg:tt)*) => { $crate::logger::log($crate::LogLevel::Info, format_args!($($arg)*)) };
}

#[macro_export]
macro_rules! warn {
    ($($arg:tt)*) => { $crate::logger::log($crate::LogLevel::Warn, format_args!($($arg)*)) };
}

#[macro_export]
macro_rules! error {
    ($($arg:tt)*) => { $crate::logger::log($crate::LogLevel::Error, format_args!($($arg)*)) };
}

pub fn test_echo() -> bool {
    log(LogLevel::Info, format_args!("logger:test:ok"));
    true
}