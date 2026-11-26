//! Logging module
//!
//! Provides unified log levels and entry definitions across the UI.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Log level
///
/// Implements `PartialOrd` for filtering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum LogLevel {
    Error = 0,
    Warn = 1,
    Info = 2,
    Debug = 3,
    Trace = 4,
}

impl LogLevel {
    /// Get the corresponding emoji icon
    pub fn icon(&self) -> &'static str {
        match self {
            LogLevel::Error => "❌",
            LogLevel::Warn => "⚠️",
            LogLevel::Info => "ℹ️",
            LogLevel::Debug => "🔍",
            LogLevel::Trace => "📝",
        }
    }

    /// Get the display name
    pub fn name(&self) -> &'static str {
        match self {
            LogLevel::Error => "ERROR",
            LogLevel::Warn => "WARN",
            LogLevel::Info => "INFO",
            LogLevel::Debug => "DEBUG",
            LogLevel::Trace => "TRACE",
        }
    }
}

impl std::str::FromStr for LogLevel {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_uppercase().as_str() {
            "ERROR" => Ok(LogLevel::Error),
            "WARN" => Ok(LogLevel::Warn),
            "INFO" => Ok(LogLevel::Info),
            "DEBUG" => Ok(LogLevel::Debug),
            "TRACE" => Ok(LogLevel::Trace),
            _ => Ok(LogLevel::Info), // Default back to Info; keep the original lenient behavior rather than returning Err
        }
    }
}

impl fmt::Display for LogLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.name())
    }
}

/// Global log entry
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LogEntry {
    pub level: LogLevel,
    pub message: String,
    // May be extended with fields such as a timestamp in the future
    // pub timestamp: SystemTime,
}

impl LogEntry {
    pub fn new(level: LogLevel, message: impl Into<String>) -> Self {
        Self {
            level,
            message: message.into(),
        }
    }
}
