//! `Std.Log`'s line where no handler takes it: written to standard error,
//! with when and at what level, if its level is at or above the one
//! `MEADOW_LOG` names.
//!
//! A level is `0` debug, `1` info, `2` warn and `3` error, as `Std.Log`'s
//! `rank` has them. `MEADOW_LOG` is `debug`, `info`, `warn`, `error` or `off`,
//! and `info` where it is unset or none of those: what a program says it is
//! doing, and what goes wrong, but not the detail a developer turns on. It is
//! read once.
//!
//! ```text
//! 2026-10-17T19:12:04.512Z INFO  listening on 127.0.0.1:8080
//! ```
//!
//! Every runtime calls [`line`], so a program logs the same on each; the line
//! is written in one call, so two threads' lines never interleave.

use std::io::Write;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

/// The level names, by rank, padded to one width so messages line up.
const NAMES: [&str; 4] = ["DEBUG", "INFO ", "WARN ", "ERROR"];

/// The least level written: [`NAMES`]'s index, or 4 for nothing.
pub fn threshold() -> i64 {
    static AT: OnceLock<i64> = OnceLock::new();
    *AT.get_or_init(|| threshold_of(std::env::var("MEADOW_LOG").ok().as_deref()))
}

/// What `MEADOW_LOG` set to `said` means.
pub fn threshold_of(said: Option<&str>) -> i64 {
    match said.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
        Some("debug") | Some("trace") | Some("all") => 0,
        Some("warn") | Some("warning") => 2,
        Some("error") => 3,
        Some("off") | Some("none") => 4,
        _ => 1,
    }
}

/// Write `message` at `level` to standard error, if the threshold lets it.
pub fn line(level: i64, message: &str) {
    if level < threshold() {
        return;
    }
    let text = format_line(SystemTime::now(), level, message);
    let mut err = std::io::stderr().lock();
    let _ = err.write_all(text.as_bytes());
    let _ = err.flush();
}

/// The line written for `message` at `level` at `when`, with its newline.
pub fn format_line(when: SystemTime, level: i64, message: &str) -> String {
    let name = NAMES[level.clamp(0, 3) as usize];
    format!("{} {name} {message}\n", rfc3339(when))
}

/// `when` in UTC as RFC 3339, to the millisecond: `2026-10-17T19:12:04.512Z`.
pub fn rfc3339(when: SystemTime) -> String {
    let since = when.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = since.as_secs() as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (y, m, d) = civil(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60,
        since.subsec_millis()
    )
}

/// The civil date `days` after 1970-01-01, by Howard Hinnant's
/// `civil_from_days`.
fn civil(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn a_line_says_when_and_at_what_level() {
        let when = UNIX_EPOCH + Duration::from_millis(1_792_264_324_512);
        assert_eq!(
            format_line(when, 1, "listening"),
            "2026-10-17T19:12:04.512Z INFO  listening\n"
        );
        assert_eq!(
            format_line(UNIX_EPOCH, 3, "x"),
            "1970-01-01T00:00:00.000Z ERROR x\n"
        );
    }

    #[test]
    fn the_threshold_is_what_meadow_log_names_and_info_otherwise() {
        assert_eq!(threshold_of(None), 1);
        assert_eq!(threshold_of(Some("DEBUG")), 0);
        assert_eq!(threshold_of(Some("warn")), 2);
        assert_eq!(threshold_of(Some("off")), 4);
        assert_eq!(threshold_of(Some("loud")), 1);
    }

    #[test]
    fn dates_are_civil_dates() {
        assert_eq!(civil(0), (1970, 1, 1));
        assert_eq!(civil(-1), (1969, 12, 31));
        assert_eq!(civil(11_016), (2000, 2, 29));
    }
}
