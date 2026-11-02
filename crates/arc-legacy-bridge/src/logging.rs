//! Messages go to stderr (which v0.7 supervisors already capture: the
//! desktop's Logs screen, launchd/systemd `node.log`) and are appended to
//! `legacy-bridge/bridge.log`. No message ever contains a seed or key.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

pub struct Log {
    file: Option<File>,
}

impl Log {
    pub fn stderr_only() -> Log {
        Log { file: None }
    }

    /// Append to `path` as well; logging never fails the bridge.
    pub fn with_file(path: &Path) -> Log {
        let file = OpenOptions::new().create(true).append(true).open(path).ok();
        Log { file }
    }

    pub fn info(&mut self, message: &str) {
        eprintln!("arc-legacy-bridge: {message}");
        self.append("info", message);
    }

    pub fn warn(&mut self, message: &str) {
        eprintln!("arc-legacy-bridge: warning: {message}");
        self.append("warn", message);
    }

    fn append(&mut self, level: &str, message: &str) {
        if let Some(file) = self.file.as_mut() {
            let _ = writeln!(file, "{} {level} {message}", utc_rfc3339(unix_now()));
        }
    }
}

pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

/// `YYYY-MM-DDTHH:MM:SSZ` for a Unix timestamp (proleptic Gregorian, UTC).
pub fn utc_rfc3339(unix: u64) -> String {
    let days = i64::try_from(unix / 86_400).unwrap_or(0);
    let seconds = unix % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        seconds / 3600,
        (seconds % 3600) / 60,
        seconds % 60
    )
}

/// Howard Hinnant's `civil_from_days`.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (
        year,
        u32::try_from(month).unwrap_or(1),
        u32::try_from(day).unwrap_or(1),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_formatting_known_answers() {
        assert_eq!(utc_rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(utc_rfc3339(951_868_800), "2000-03-01T00:00:00Z");
        assert_eq!(utc_rfc3339(1_709_210_096), "2024-02-29T12:34:56Z");
        assert_eq!(utc_rfc3339(4_102_444_799), "2099-12-31T23:59:59Z");
    }
}
