//! [`ReleaseDate`]: the calendar date a model was released.
//!
//! Operators declare it per model (`release_date = "2024-05-13"`) so clients
//! can sort `GET /v1/models` by release. The gateway never introspects
//! upstreams, so this is purely operator-supplied metadata. It is a plain
//! civil date (no time, no zone): ordering is chronological, and
//! [`ReleaseDate::unix_seconds`] maps it to midnight UTC for the
//! OpenAI-compatible integer `created` field.

use std::fmt;
use std::str::FromStr;

use serde::de::{self, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A model's release date, written and read as ISO 8601 `YYYY-MM-DD`.
///
/// Field order (year, month, day) makes the derived `Ord` chronological.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ReleaseDate {
    year: u16,
    month: u8,
    day: u8,
}

/// Why a string is not a valid [`ReleaseDate`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid release date '{0}': expected an ISO 8601 date YYYY-MM-DD between 1970-01-01 and 9999-12-31")]
pub struct ReleaseDateError(String);

impl ReleaseDate {
    /// Build a date from its parts, rejecting impossible dates (month 13,
    /// February 30, ...) and years before the Unix epoch.
    ///
    /// # Errors
    /// [`ReleaseDateError`] when the parts do not form a real date in
    /// 1970..=9999.
    pub fn new(year: u16, month: u8, day: u8) -> Result<Self, ReleaseDateError> {
        let valid = (1970..=9999).contains(&year)
            && (1..=12).contains(&month)
            && day >= 1
            && day <= days_in_month(year, month);
        if valid {
            Ok(Self { year, month, day })
        } else {
            Err(ReleaseDateError(format!("{year:04}-{month:02}-{day:02}")))
        }
    }

    /// Seconds since the Unix epoch at 00:00:00 UTC on this date.
    #[must_use]
    pub fn unix_seconds(self) -> u64 {
        days_since_epoch(self.year, self.month, self.day) * 86_400
    }
}

impl FromStr for ReleaseDate {
    type Err = ReleaseDateError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // Echo at most a short prefix: the message can reach an admin API
        // response, and an arbitrarily long bad value adds nothing there.
        let err = || ReleaseDateError(s.chars().take(32).collect());
        // Strict shape: exactly `DDDD-DD-DD`, ASCII digits only.
        let bytes = s.as_bytes();
        let shape_ok = bytes.len() == 10
            && bytes[4] == b'-'
            && bytes[7] == b'-'
            && bytes
                .iter()
                .enumerate()
                .all(|(i, b)| i == 4 || i == 7 || b.is_ascii_digit());
        if !shape_ok {
            return Err(err());
        }
        let year = s[0..4].parse().map_err(|_| err())?;
        let month = s[5..7].parse().map_err(|_| err())?;
        let day = s[8..10].parse().map_err(|_| err())?;
        Self::new(year, month, day).map_err(|_| err())
    }
}

impl fmt::Display for ReleaseDate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }
}

impl Serialize for ReleaseDate {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

/// The key the `toml` crate (and figment, which uses it) wraps a native TOML
/// date literal in when handing it to serde as a one-entry map.
const TOML_DATETIME_KEY: &str = "$__toml_private_datetime";

/// Accepts a `"YYYY-MM-DD"` string, and also an unquoted TOML date literal
/// (`release_date = 2024-05-13`), which is the natural way to write a date in
/// TOML. Either way the value goes through the same strict [`FromStr`], so a
/// TOML datetime or time literal is still rejected.
impl<'de> Deserialize<'de> for ReleaseDate {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ReleaseDateVisitor;

        impl<'de> Visitor<'de> for ReleaseDateVisitor {
            type Value = ReleaseDate;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("an ISO 8601 date \"YYYY-MM-DD\"")
            }

            fn visit_str<E: de::Error>(self, s: &str) -> Result<ReleaseDate, E> {
                s.parse().map_err(E::custom)
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<ReleaseDate, A::Error> {
                match map.next_entry::<String, String>()? {
                    Some((key, value)) if key == TOML_DATETIME_KEY => {
                        value.parse().map_err(de::Error::custom)
                    }
                    _ => Err(de::Error::invalid_type(de::Unexpected::Map, &self)),
                }
            }
        }

        deserializer.deserialize_any(ReleaseDateVisitor)
    }
}

const fn is_leap(year: u16) -> bool {
    (year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400)
}

const fn days_in_month(year: u16, month: u8) -> u8 {
    match month {
        2 if is_leap(year) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Days from 1970-01-01 to the given (already validated, >= 1970) date, in
/// constant time (Howard Hinnant's `days_from_civil`). Unsigned arithmetic is
/// safe because validation guarantees the result is non-negative.
fn days_since_epoch(year: u16, month: u8, day: u8) -> u64 {
    let (m, d) = (u64::from(month), u64::from(day));
    // Shift the year to start in March so the leap day ends it.
    let y = u64::from(year) - u64::from(month <= 2);
    let era = y / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(s: &str) -> ReleaseDate {
        s.parse().expect("valid date")
    }

    #[test]
    fn parses_and_round_trips_iso_dates() {
        for s in ["1970-01-01", "2024-05-13", "2024-02-29", "9999-12-31"] {
            assert_eq!(date(s).to_string(), s);
        }
    }

    #[test]
    fn rejects_malformed_and_impossible_dates() {
        for s in [
            "",
            "2024-5-13",
            "2024/05/13",
            "13-05-2024",
            "2024-05-13T00:00:00Z",
            " 2024-05-13",
            "+024-05-13",
            "2024-00-10",
            "2024-13-01",
            "2024-04-31",
            "2023-02-29",
            "1900-02-28",
            "1969-12-31",
            "2024-05-00",
        ] {
            assert!(s.parse::<ReleaseDate>().is_err(), "{s:?} must be rejected");
        }
    }

    #[test]
    fn unix_seconds_is_midnight_utc() {
        assert_eq!(date("1970-01-01").unix_seconds(), 0);
        assert_eq!(date("1970-01-02").unix_seconds(), 86_400);
        // Cross-checked with `date -u -d 2024-05-13 +%s`.
        assert_eq!(date("2024-05-13").unix_seconds(), 1_715_558_400);
        assert_eq!(date("2000-03-01").unix_seconds(), 951_868_800);
    }

    #[test]
    fn unix_seconds_matches_a_day_by_day_count() {
        // Brute-force oracle over a leap-rich span, including century years.
        let mut expected = 0_u64;
        for year in 1970..=2104_u16 {
            for month in 1..=12_u8 {
                for day in 1..=days_in_month(year, month) {
                    let d = ReleaseDate::new(year, month, day).unwrap();
                    assert_eq!(d.unix_seconds(), expected * 86_400, "{d}");
                    expected += 1;
                }
            }
        }
        assert_eq!(date("9999-12-31").unix_seconds(), 253_402_214_400);
    }

    #[test]
    fn error_echo_is_bounded() {
        let long = "x".repeat(10_000);
        let err = long.parse::<ReleaseDate>().unwrap_err().to_string();
        assert!(err.len() < 200, "{err}");
    }

    #[test]
    fn ordering_is_chronological() {
        let mut dates = [date("2024-05-13"), date("2023-12-31"), date("2024-01-02")];
        dates.sort();
        let sorted: Vec<String> = dates.iter().map(ToString::to_string).collect();
        assert_eq!(sorted, ["2023-12-31", "2024-01-02", "2024-05-13"]);
    }

    #[test]
    fn serde_uses_the_iso_string() {
        let json = serde_json::to_string(&date("2024-05-13")).unwrap();
        assert_eq!(json, "\"2024-05-13\"");
        let back: ReleaseDate = serde_json::from_str(&json).unwrap();
        assert_eq!(back, date("2024-05-13"));
        let bad = serde_json::from_str::<ReleaseDate>("\"2024-02-30\"").unwrap_err();
        assert!(bad
            .to_string()
            .contains("invalid release date '2024-02-30'"));
    }
}
