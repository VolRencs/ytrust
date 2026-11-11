use std::time::{SystemTime, UNIX_EPOCH};

/// Current Unix timestamp in seconds. Returns 0 if the clock is before epoch.
pub fn unix_time() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

pub fn unix_time_i64() -> i64 {
    i64::try_from(unix_time()).unwrap_or(i64::MAX)
}
