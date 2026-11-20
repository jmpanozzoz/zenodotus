//! Wall-clock time: Unix-second timestamps, RFC 3339 rendering and an injectable clock.

use std::fmt;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Seconds in one day.
const SECONDS_PER_DAY: i64 = 86_400;

/// A point in time as whole seconds since the Unix epoch, rendered as RFC 3339 UTC.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Timestamp(i64);

impl Timestamp {
    /// The timestamp `seconds` after the Unix epoch; negative values are before it.
    #[must_use]
    pub const fn from_unix(seconds: i64) -> Self {
        Self(seconds)
    }

    /// Seconds since the Unix epoch.
    #[must_use]
    pub const fn unix(self) -> i64 {
        self.0
    }
}

impl fmt::Display for Timestamp {
    /// Formats as `YYYY-MM-DDTHH:MM:SSZ`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let days = self.0.div_euclid(SECONDS_PER_DAY);
        let second_of_day = self.0.rem_euclid(SECONDS_PER_DAY);
        let (year, month, day) = civil_from_days(days);
        write!(
            f,
            "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
            second_of_day / 3600,
            second_of_day % 3600 / 60,
            second_of_day % 60
        )
    }
}

/// Converts days since 1970-01-01 into a proleptic Gregorian `(year, month, day)`.
///
/// This is Howard Hinnant's `civil_from_days`: eras are 400-year cycles of 146,097 days, counted from 0000-03-01 so
/// that the leap day falls at the end of each year.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era = (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// A source of the current wall-clock time.
pub trait Clock: Send + Sync {
    /// The current time.
    fn now(&self) -> Timestamp;
}

/// The operating system clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Timestamp {
        Timestamp(seconds_since_epoch(SystemTime::now()))
    }
}

/// Whole seconds between the Unix epoch and `time`, negative before it, saturating at the `i64` range.
fn seconds_since_epoch(time: SystemTime) -> i64 {
    match time.duration_since(UNIX_EPOCH) {
        Ok(after) => i64::try_from(after.as_secs()).unwrap_or(i64::MAX),
        Err(before) => i64::try_from(before.duration().as_secs()).map_or(i64::MIN, |s| -s),
    }
}

/// A clock that returns a time set by the caller, for tests.
#[derive(Debug, Default)]
pub struct FixedClock(AtomicI64);

impl FixedClock {
    /// A clock stopped at `at`.
    #[must_use]
    pub fn new(at: Timestamp) -> Self {
        Self(AtomicI64::new(at.0))
    }

    /// Moves the clock forward by `seconds`.
    pub fn advance(&self, seconds: i64) {
        self.0.fetch_add(seconds, Ordering::SeqCst);
    }
}

impl Clock for FixedClock {
    fn now(&self) -> Timestamp {
        Timestamp(self.0.load(Ordering::SeqCst))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn rfc3339(seconds: i64) -> String {
        Timestamp::from_unix(seconds).to_string()
    }

    #[test]
    fn formats_known_dates_as_rfc3339_utc() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(rfc3339(1_709_210_096), "2024-02-29T12:34:56Z");
        assert_eq!(rfc3339(1_709_251_199), "2024-02-29T23:59:59Z");
        assert_eq!(rfc3339(1_709_251_200), "2024-03-01T00:00:00Z");
        assert_eq!(rfc3339(4_107_456_000), "2100-02-28T00:00:00Z");
        assert_eq!(rfc3339(4_107_542_400), "2100-03-01T00:00:00Z");
        assert_eq!(rfc3339(253_402_300_799), "9999-12-31T23:59:59Z");
    }

    #[test]
    fn formats_times_before_the_epoch() {
        assert_eq!(rfc3339(-1), "1969-12-31T23:59:59Z");
        assert_eq!(rfc3339(-86_400), "1969-12-31T00:00:00Z");
        assert_eq!(rfc3339(-2_203_891_200), "1900-03-01T00:00:00Z");
    }

    #[test]
    fn exposes_its_unix_seconds_and_orders_by_them() {
        assert_eq!(Timestamp::from_unix(42).unix(), 42);
        assert!(Timestamp::from_unix(1) < Timestamp::from_unix(2));
    }

    #[test]
    fn serializes_as_plain_seconds() {
        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct Wrapper {
            at: Timestamp,
        }
        let text = toml::to_string(&Wrapper {
            at: Timestamp::from_unix(7),
        })
        .unwrap();
        assert_eq!(text.trim(), "at = 7");
        assert_eq!(toml::from_str::<Wrapper>(&text).unwrap().at, Timestamp::from_unix(7));
    }

    #[test]
    fn the_fixed_clock_returns_what_it_was_given_and_advances() {
        let clock = FixedClock::new(Timestamp::from_unix(100));
        assert_eq!(clock.now(), Timestamp::from_unix(100));
        clock.advance(5);
        assert_eq!(clock.now(), Timestamp::from_unix(105));
    }

    #[test]
    fn the_system_clock_is_after_2024() {
        assert!(SystemClock.now() > Timestamp::from_unix(1_704_067_200));
    }

    #[test]
    fn seconds_since_epoch_handles_both_sides_of_the_epoch() {
        assert_eq!(seconds_since_epoch(UNIX_EPOCH + Duration::from_secs(9)), 9);
        assert_eq!(seconds_since_epoch(UNIX_EPOCH - Duration::from_secs(9)), -9);
        let latest = UNIX_EPOCH + Duration::from_secs(u64::try_from(i64::MAX).unwrap());
        assert_eq!(seconds_since_epoch(latest), i64::MAX);
    }
}
