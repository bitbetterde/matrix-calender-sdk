//! Signed ISO 8601 durations (RFC 8984 §1.4.6 `SignedDuration`).
//!
//! Used by [`AlertTrigger::Offset`](super::alert::AlertTrigger::Offset) to express an offset
//! from an event's start or end. The wire format is a string: an ISO 8601 duration with an
//! optional leading `-`.
//!
//! ```text
//! -PT15M     15 minutes before
//! PT0S       at the event
//! -P1DT9H    1 day and 9 hours before
//! P1W        1 week after
//! ```
//!
//! The grammar is `["-"] "P" (nW | nD ["T" (nH) (nM) (nS)] | "T" (nH) (nM) (nS))`: weeks stand
//! alone, at least one component is required, components appear at most once and in descending
//! order, and only seconds may have a fraction. Parsing is strict about everything else
//! (uppercase designators, no `+` sign, no whitespace) but does not require a lower component
//! to be present to allow a higher one, so `PT1H30S` is accepted as well.
//!
//! Values are kept with second precision: fractional seconds are accepted on input (RFC 8984
//! allows them) and truncated towards zero.

use std::{fmt, str::FromStr};

use chrono::TimeDelta;
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

use crate::{Error, Result};

const SECONDS_PER_MINUTE: i64 = 60;
const SECONDS_PER_HOUR: i64 = 60 * SECONDS_PER_MINUTE;
const SECONDS_PER_DAY: i64 = 24 * SECONDS_PER_HOUR;
const SECONDS_PER_WEEK: i64 = 7 * SECONDS_PER_DAY;

/// A signed duration with second precision, serialized as an ISO 8601 duration string.
///
/// [`Display`](fmt::Display) writes the canonical form ([`SignedDuration::ZERO`] is `PT0S`, a
/// whole number of weeks is written as weeks, everything else as days and a time part without
/// zero components) and [`FromStr`] parses it back, so every value round-trips; `P7D` and
/// `PT90M` parse and format back as `P1W` and `PT1H30M`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SignedDuration(TimeDelta);

impl SignedDuration {
    /// The zero duration (`PT0S`).
    pub const ZERO: Self = Self(TimeDelta::zero());

    /// The duration of `delta`, with sub-second precision truncated towards zero.
    pub const fn new(delta: TimeDelta) -> Self {
        match TimeDelta::try_seconds(delta.num_seconds()) {
            Some(whole) => Self(whole),
            // The whole seconds of a `TimeDelta` are always a valid `TimeDelta`.
            None => Self(delta),
        }
    }

    /// A duration of `seconds` seconds.
    ///
    /// Fails with [`Error::InvalidDuration`] if `seconds` is outside the range `chrono` can
    /// represent (roughly ±292 million years).
    pub fn from_seconds(seconds: i64) -> Result<Self> {
        TimeDelta::try_seconds(seconds)
            .map(Self)
            .ok_or_else(|| Error::InvalidDuration(format!("{seconds} seconds is out of range")))
    }

    /// This duration as a [`TimeDelta`].
    pub const fn as_time_delta(&self) -> TimeDelta {
        self.0
    }

    /// The number of whole seconds, negative for a duration before the reference point.
    pub const fn num_seconds(&self) -> i64 {
        self.0.num_seconds()
    }

    /// Whether this duration is zero (`PT0S`).
    pub const fn is_zero(&self) -> bool {
        self.0.is_zero()
    }

    /// Whether this duration points into the past, i.e. carries the leading `-`.
    pub const fn is_negative(&self) -> bool {
        self.0.num_seconds() < 0
    }
}

impl From<TimeDelta> for SignedDuration {
    /// Truncates sub-second precision, see [`SignedDuration::new`].
    fn from(delta: TimeDelta) -> Self {
        Self::new(delta)
    }
}

impl From<SignedDuration> for TimeDelta {
    fn from(duration: SignedDuration) -> Self {
        duration.0
    }
}

impl fmt::Display for SignedDuration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let total = self.num_seconds();
        if total == 0 {
            return f.write_str("PT0S");
        }
        if total < 0 {
            f.write_str("-")?;
        }
        f.write_str("P")?;
        // Within `TimeDelta`'s range, so negating cannot overflow.
        let abs = self.0.abs().num_seconds();
        if abs % SECONDS_PER_WEEK == 0 {
            return write!(f, "{}W", abs / SECONDS_PER_WEEK);
        }
        let days = abs / SECONDS_PER_DAY;
        if days > 0 {
            write!(f, "{days}D")?;
        }
        let rest = abs % SECONDS_PER_DAY;
        if rest > 0 {
            f.write_str("T")?;
            let hours = rest / SECONDS_PER_HOUR;
            let minutes = (rest % SECONDS_PER_HOUR) / SECONDS_PER_MINUTE;
            let seconds = rest % SECONDS_PER_MINUTE;
            if hours > 0 {
                write!(f, "{hours}H")?;
            }
            if minutes > 0 {
                write!(f, "{minutes}M")?;
            }
            if seconds > 0 {
                write!(f, "{seconds}S")?;
            }
        }
        Ok(())
    }
}

impl FromStr for SignedDuration {
    type Err = Error;

    /// Parses an ISO 8601 duration as RFC 8984 §1.4.6 defines it (see the
    /// [module documentation](self)).
    ///
    /// # Errors
    ///
    /// [`Error::InvalidDuration`] if `s` does not match the grammar or does not fit into a
    /// [`TimeDelta`].
    fn from_str(s: &str) -> Result<Self> {
        parse(s)
    }
}

impl Serialize for SignedDuration {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for SignedDuration {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        parse(&s).map_err(de::Error::custom)
    }
}

/// Parses the wire format; the error messages quote the whole input.
fn parse(s: &str) -> Result<SignedDuration> {
    let invalid = |reason: &str| Error::InvalidDuration(format!("{s}: {reason}"));

    let (negative, body) = match s.strip_prefix('-') {
        Some(body) => (true, body),
        None => (false, s),
    };
    let body = body
        .strip_prefix('P')
        .ok_or_else(|| invalid("expected a leading `P` (or `-P`)"))?;
    let (date_part, time_part) = match body.split_once('T') {
        Some((date, time)) => (date, Some(time)),
        None => (body, None),
    };

    let mut seconds = 0i64;
    let mut components = 0usize;
    let add = |total: i64, value: i64, unit: i64| -> Result<i64> {
        value
            .checked_mul(unit)
            .and_then(|added| total.checked_add(added))
            .ok_or_else(|| invalid("duration is out of range"))
    };

    if !date_part.is_empty() {
        let (digits, rest) = split_digits(date_part);
        let value =
            whole(digits).ok_or_else(|| invalid("expected a whole number of days or weeks"))?;
        match rest.as_bytes() {
            [b'W'] if time_part.is_none() => seconds = add(seconds, value, SECONDS_PER_WEEK)?,
            [b'W'] => return Err(invalid("weeks cannot be combined with a time part")),
            [b'D'] => seconds = add(seconds, value, SECONDS_PER_DAY)?,
            _ => return Err(invalid("expected `nD` or `nW` before the time part")),
        }
        components += 1;
    }

    if let Some(time_part) = time_part {
        if time_part.is_empty() {
            return Err(invalid("`T` must be followed by a time component"));
        }
        let mut rest = time_part;
        // Hours, minutes and seconds, each at most once and in this order.
        let mut units: &[(u8, i64)] = &[
            (b'H', SECONDS_PER_HOUR),
            (b'M', SECONDS_PER_MINUTE),
            (b'S', 1),
        ];
        while !rest.is_empty() {
            let (digits, tail) = split_digits(rest);
            let (&unit, consumed) = tail
                .as_bytes()
                .first()
                .and_then(|unit| {
                    let position = units.iter().position(|(u, _)| u == unit)?;
                    Some((unit, position + 1))
                })
                .ok_or_else(|| invalid("expected `nH`, `nM` or `nS`, in this order"))?;
            let (_, factor) = units[consumed - 1];
            units = &units[consumed..];
            let value = if unit == b'S' {
                truncated_seconds(digits)
                    .ok_or_else(|| invalid("expected a number of seconds, fractions allowed"))?
            } else {
                whole(digits).ok_or_else(|| {
                    invalid("only seconds may have a fraction, hours and minutes are whole")
                })?
            };
            seconds = add(seconds, value, factor)?;
            rest = &tail[1..];
            components += 1;
        }
    }

    if components == 0 {
        return Err(invalid("a duration needs at least one component"));
    }
    if negative {
        seconds = seconds
            .checked_neg()
            .ok_or_else(|| invalid("duration is out of range"))?;
    }
    TimeDelta::try_seconds(seconds)
        .map(SignedDuration)
        .ok_or_else(|| invalid("duration is out of range"))
}

/// Splits the leading digits (and the dot of a fraction) off `s`.
fn split_digits(s: &str) -> (&str, &str) {
    let end = s
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(s.len());
    s.split_at(end)
}

/// `digits` as a whole number, or `None` if it is empty, fractional or too large.
fn whole(digits: &str) -> Option<i64> {
    digits.parse().ok()
}

/// `digits` as a number of seconds with the fraction truncated, or `None` if it is malformed.
fn truncated_seconds(digits: &str) -> Option<i64> {
    let (whole_part, fraction) = match digits.split_once('.') {
        Some((whole_part, fraction)) => (whole_part, Some(fraction)),
        None => (digits, None),
    };
    if let Some(fraction) = fraction
        && (fraction.is_empty() || !fraction.bytes().all(|b| b.is_ascii_digit()))
    {
        return None;
    }
    whole(whole_part)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(s: &str) -> SignedDuration {
        s.parse().expect("valid duration")
    }

    #[test]
    fn parses_every_component() {
        assert_eq!(parsed("PT0S").num_seconds(), 0);
        assert_eq!(parsed("-PT15M").num_seconds(), -15 * 60);
        assert_eq!(parsed("PT1H").num_seconds(), 3600);
        assert_eq!(parsed("-PT1H30M").num_seconds(), -5400);
        assert_eq!(parsed("PT1H30S").num_seconds(), 3630);
        assert_eq!(parsed("PT90M").num_seconds(), 5400);
        assert_eq!(parsed("P1D").num_seconds(), 86_400);
        assert_eq!(parsed("-P1DT9H").num_seconds(), -(86_400 + 9 * 3600));
        assert_eq!(parsed("P6DT15H").num_seconds(), 6 * 86_400 + 15 * 3600);
        assert_eq!(parsed("P1W").num_seconds(), 604_800);
        assert_eq!(parsed("P2W").num_seconds(), 1_209_600);
        assert_eq!(parsed("P0D").num_seconds(), 0);
        assert_eq!(parsed("-PT0S"), SignedDuration::ZERO);
        assert_eq!(parsed("PT1H2M3S").num_seconds(), 3723);
        assert_eq!(parsed("P1DT1H2M3S").num_seconds(), 86_400 + 3723);
        assert_eq!(parsed("PT0.5S").num_seconds(), 0);
        assert_eq!(parsed("PT1.999S").num_seconds(), 1);
        assert_eq!(parsed("-PT1.5S").num_seconds(), -1);
    }

    #[test]
    fn formats_canonically_and_round_trips() {
        for s in [
            "PT0S",
            "-PT15M",
            "-P1DT9H",
            "P6DT15H",
            "-PT1H30M",
            "P1W",
            "-P2W",
            "PT1H",
            "PT30S",
            "P3D",
            "P1DT1H2M3S",
            "PT1H30S",
            "-PT59S",
        ] {
            let duration = parsed(s);
            assert_eq!(duration.to_string(), s, "{s} is not canonical");
            assert_eq!(parsed(&duration.to_string()), duration);
        }
        // Non-canonical but valid input formats as the canonical equivalent.
        assert_eq!(parsed("P7D").to_string(), "P1W");
        assert_eq!(parsed("PT90M").to_string(), "PT1H30M");
        assert_eq!(parsed("P0D").to_string(), "PT0S");
        assert_eq!(parsed("PT0.25S").to_string(), "PT0S");
        assert_eq!(parsed("P1DT0H0M0S").to_string(), "P1D");
    }

    #[test]
    fn rejects_malformed_durations() {
        for s in [
            "", "-", "P", "PT", "-P", "-PT", "15M", "T15M", "PT1.5H", "PT1.5M", "P1.5D", "P1.5W",
            "P1W2D", "P1WT1H", "PT1S30M", "PT1H1H", "P1D1D", "PD", "PTM", "PT.5S", "PT5.S",
            "+PT15M", "pt15m", "p1d", "PT15M ", " PT15M", "PT15Q", "P1DT1HX", "PT1H30M0", "PTT1H",
            "PT1HT1M", "P1DT", "PT--1S", "P-1D",
        ] {
            assert!(
                s.parse::<SignedDuration>().is_err(),
                "{s:?} should be rejected"
            );
        }
        assert!(matches!(
            "nonsense".parse::<SignedDuration>(),
            Err(Error::InvalidDuration(_))
        ));
    }

    #[test]
    fn rejects_out_of_range_durations() {
        assert!("P999999999999999999999D".parse::<SignedDuration>().is_err());
        assert!("P9999999999999999W".parse::<SignedDuration>().is_err());
        assert!(SignedDuration::from_seconds(i64::MAX).is_err());
        assert_eq!(
            SignedDuration::from_seconds(-900).expect("in range"),
            parsed("-PT15M")
        );
    }

    #[test]
    fn conversions_truncate_sub_second_precision() {
        let duration = SignedDuration::new(TimeDelta::milliseconds(-1_500));
        assert_eq!(duration.num_seconds(), -1);
        assert_eq!(duration.to_string(), "-PT1S");
        assert!(duration.is_negative());
        assert_eq!(
            TimeDelta::from(parsed("PT15M")),
            TimeDelta::try_minutes(15).expect("in range")
        );
        assert_eq!(
            SignedDuration::from(TimeDelta::try_hours(2).expect("in range")).to_string(),
            "PT2H"
        );
        assert!(SignedDuration::ZERO.is_zero());
        assert!(!SignedDuration::ZERO.is_negative());
        assert_eq!(SignedDuration::default(), SignedDuration::ZERO);
        assert_eq!(
            parsed("-PT15M").as_time_delta(),
            TimeDelta::try_minutes(-15).expect("in range")
        );
    }

    #[test]
    fn serde_uses_the_wire_string() {
        let duration: SignedDuration = serde_json::from_value(json("-PT15M")).expect("valid");
        assert_eq!(duration.num_seconds(), -900);
        assert_eq!(
            serde_json::to_value(duration).expect("serializable"),
            json("-PT15M")
        );
        assert!(serde_json::from_value::<SignedDuration>(json("PT1.5H")).is_err());
        assert!(serde_json::from_value::<SignedDuration>(serde_json::json!(-900)).is_err());
    }

    fn json(s: &str) -> serde_json::Value {
        serde_json::Value::String(s.to_owned())
    }

    #[test]
    fn durations_are_ordered_by_length() {
        let mut durations = [parsed("PT1H"), parsed("-PT15M"), parsed("PT0S")];
        durations.sort();
        assert_eq!(
            durations,
            [parsed("-PT15M"), parsed("PT0S"), parsed("PT1H")]
        );
    }
}
