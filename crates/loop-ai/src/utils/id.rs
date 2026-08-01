//! Identifiers and timestamps.

use uuid::Uuid;

/// Current unix epoch milliseconds.
pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Generate a UUID v7 string.
pub fn new_id() -> String {
    Uuid::now_v7().to_string()
}
