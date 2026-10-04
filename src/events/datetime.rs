//! Date/time representation for calendar events (MSC4496 §2.1, §2.2).
//!
//! Timed values store the UTC instant **and** the wall-clock time in a named IANA timezone,
//! mirroring JSCalendar. All-day values store only a date.
//!
//! Years are limited to [`MIN_YEAR`]`..=`[`MAX_YEAR`] (the range of iCalendar and RFC 3339):
//! deserialization and the fallible constructors reject anything else, so that date
//! arithmetic on remote data cannot overflow.

use std::fmt;

use chrono::{
    DateTime, Datelike, NaiveDate, NaiveDateTime, NaiveTime, Offset, TimeDelta, TimeZone, Timelike,
    Utc,
};
use chrono_tz::Tz;
use serde::{Deserialize, Deserializer, Serialize, de};

use crate::{Error, Result};

/// The wire format of the `utc` field and of all other UTC timestamps in MSC4496.
pub const UTC_FORMAT: &str = "%Y-%m-%dT%H:%M:%SZ";

/// The wire format of the `localtime` field.
pub const LOCALTIME_FORMAT: &str = "%Y-%m-%dT%H:%M:%S";

/// The wire format of all-day `date` fields.
pub const DATE_FORMAT: &str = "%Y-%m-%d";

/// The earliest supported year.
pub const MIN_YEAR: i32 = 1;

/// The latest supported year.
pub const MAX_YEAR: i32 = 9999;

/// Checks that `date` lies within [`MIN_YEAR`]`..=`[`MAX_YEAR`].
pub fn check_year(date: NaiveDate) -> Result<()> {
    if (MIN_YEAR..=MAX_YEAR).contains(&date.year()) {
        Ok(())
    } else {
        Err(Error::InvalidDateTime(format!(
            "year {} is outside the supported range {MIN_YEAR}..={MAX_YEAR}",
            date.year()
        )))
    }
}

/// A point in time used for an event's `start` or `end`.
///
/// Serializes to exactly one of the two MSC shapes:
///
/// ```json
/// { "utc": "2026-07-01T07:00:00Z", "localtime": "2026-07-01T09:00:00", "timezone": "Europe/Berlin" }
/// { "date": "2026-12-25" }
/// ```
///
/// Deserialization is structural only: it checks that the timezone is a known IANA identifier
/// and the timestamps parse with years in [`MIN_YEAR`]`..=`[`MAX_YEAR`], but not that `utc`
/// and `localtime` agree. Call [`EventTime::validate`] for that.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(untagged)]
pub enum EventTime {
    /// A timed value.
    Timed {
        /// The UTC instant.
        #[serde(serialize_with = "utc_serde::serialize")]
        utc: DateTime<Utc>,
        /// The wall-clock time in `timezone`.
        #[serde(serialize_with = "serialize_localtime")]
        localtime: NaiveDateTime,
        /// The IANA timezone `localtime` is expressed in.
        timezone: Tz,
    },
    /// An all-day value (a calendar date without time or timezone).
    AllDay {
        /// The date.
        date: NaiveDate,
    },
}

impl EventTime {
    /// Creates a timed value from a wall-clock time in `tz`.
    ///
    /// Sub-second precision is dropped. If `local` is ambiguous (a DST fall-back overlap) the
    /// earlier instant is chosen. If `local` does not exist in `tz` (a DST spring-forward gap)
    /// or its year (or that of the resulting UTC instant) is outside
    /// [`MIN_YEAR`]`..=`[`MAX_YEAR`], an [`Error::InvalidDateTime`] is returned.
    pub fn timed(local: NaiveDateTime, tz: Tz) -> Result<Self> {
        check_year(local.date())?;
        let local = truncate_naive(local);
        let utc = tz
            .from_local_datetime(&local)
            .earliest()
            .ok_or_else(|| {
                Error::InvalidDateTime(format!("{local} does not exist in timezone {}", tz.name()))
            })?
            .with_timezone(&Utc);
        check_year(utc.naive_utc().date())?;
        Ok(Self::Timed {
            utc,
            localtime: local,
            timezone: tz,
        })
    }

    /// Creates a timed value from a UTC instant, computing the wall-clock time in `tz`.
    ///
    /// Sub-second precision is dropped. The year is not checked (this constructor cannot
    /// fail); [`EventTime::validate`] rejects values outside [`MIN_YEAR`]`..=`[`MAX_YEAR`].
    pub fn from_utc(utc: DateTime<Utc>, tz: Tz) -> Self {
        let utc = truncate_utc(utc);
        let localtime = local_naive(utc, tz);
        Self::Timed {
            utc,
            localtime,
            timezone: tz,
        }
    }

    /// Creates an all-day value.
    ///
    /// The year is not checked (this constructor cannot fail); use
    /// [`EventTime::try_all_day`] or [`EventTime::validate`] to reject values outside
    /// [`MIN_YEAR`]`..=`[`MAX_YEAR`].
    pub fn all_day(date: NaiveDate) -> Self {
        Self::AllDay { date }
    }

    /// Creates an all-day value, failing with [`Error::InvalidDateTime`] if the year is outside
    /// [`MIN_YEAR`]`..=`[`MAX_YEAR`].
    pub fn try_all_day(date: NaiveDate) -> Result<Self> {
        check_year(date)?;
        Ok(Self::AllDay { date })
    }

    /// Checks that all dates of this value (`date`, or both `utc` and `localtime`) have years
    /// within [`MIN_YEAR`]`..=`[`MAX_YEAR`].
    pub fn check_year_range(&self) -> Result<()> {
        match self {
            Self::Timed { utc, localtime, .. } => {
                check_year(utc.naive_utc().date())?;
                check_year(localtime.date())
            }
            Self::AllDay { date } => check_year(*date),
        }
    }

    /// Whether this is an all-day value.
    pub fn is_all_day(&self) -> bool {
        matches!(self, Self::AllDay { .. })
    }

    /// The timezone of a timed value, `None` for all-day values.
    pub fn timezone(&self) -> Option<Tz> {
        match self {
            Self::Timed { timezone, .. } => Some(*timezone),
            Self::AllDay { .. } => None,
        }
    }

    /// The local calendar date: the date part of `localtime`, or `date` for all-day values.
    pub fn local_date(&self) -> NaiveDate {
        match self {
            Self::Timed { localtime, .. } => localtime.date(),
            Self::AllDay { date } => *date,
        }
    }

    /// Checks that `utc` and `localtime` describe the same instant in `timezone`, and that
    /// the years are within [`MIN_YEAR`]`..=`[`MAX_YEAR`] (see
    /// [`EventTime::check_year_range`]).
    pub fn validate(&self) -> Result<()> {
        self.check_year_range()?;
        match self {
            Self::Timed {
                utc,
                localtime,
                timezone,
            } => {
                let expected = local_naive(*utc, *timezone);
                if expected == *localtime {
                    Ok(())
                } else {
                    Err(Error::InconsistentDateTime(format!(
                        "utc {} is {} in {}, but localtime is {}",
                        format_utc(utc),
                        expected.format(LOCALTIME_FORMAT),
                        timezone.name(),
                        localtime.format(LOCALTIME_FORMAT),
                    )))
                }
            }
            Self::AllDay { .. } => Ok(()),
        }
    }

    /// The UTC instant of this value.
    ///
    /// For timed values this is the stored `utc`. All-day values resolve to midnight at the
    /// start of `date` in `fallback_tz` (typically the user's or the calendar's timezone); if
    /// midnight does not exist there, the first instant after the gap is used.
    pub fn to_utc(&self, fallback_tz: Tz) -> DateTime<Utc> {
        match self {
            Self::Timed { utc, .. } => *utc,
            Self::AllDay { date } => {
                resolve_local_lenient(date.and_time(NaiveTime::MIN), fallback_tz)
            }
        }
    }
}

/// Resolves a wall-clock time in `tz` to UTC without failing (or panicking).
///
/// Ambiguous times resolve to the earlier instant. Non-existent times (DST gaps) are
/// interpreted using the UTC offset in effect before the gap, as RFC 5545 §3.3.5 prescribes,
/// which yields an instant after the gap. At the very edges of chrono's range, where the
/// offset cannot be applied, `local` is read as UTC.
pub fn resolve_local_lenient(local: NaiveDateTime, tz: Tz) -> DateTime<Utc> {
    if let Some(dt) = tz.from_local_datetime(&local).earliest() {
        return dt.with_timezone(&Utc);
    }
    // Offset in effect well before the gap (no real-world gap is longer than a day).
    let probe = local
        .checked_sub_signed(TimeDelta::hours(26))
        .unwrap_or(local);
    let offset = tz.offset_from_utc_datetime(&probe).fix();
    let utc_naive = local.checked_sub_offset(offset).unwrap_or(local);
    Utc.from_utc_datetime(&utc_naive)
}

/// The wall-clock time of `utc` in `tz`, like `utc.with_timezone(&tz).naive_local()` but
/// without panicking at the edges of chrono's range (where the UTC value is returned).
pub(crate) fn local_naive(utc: DateTime<Utc>, tz: Tz) -> NaiveDateTime {
    let naive = utc.naive_utc();
    let offset = tz.offset_from_utc_datetime(&naive).fix();
    naive.checked_add_offset(offset).unwrap_or(naive)
}

/// Parses an IANA timezone identifier.
pub fn parse_timezone(s: &str) -> Result<Tz> {
    s.parse::<Tz>()
        .map_err(|_| Error::InvalidTimezone(s.to_owned()))
}

/// Formats a UTC instant in the MSC wire format (`YYYY-MM-DDTHH:MM:SSZ`).
pub fn format_utc(dt: &DateTime<Utc>) -> String {
    dt.format(UTC_FORMAT).to_string()
}

/// Parses an RFC 3339 timestamp (any offset) into UTC.
///
/// Fails if the UTC year is outside [`MIN_YEAR`]`..=`[`MAX_YEAR`].
pub fn parse_utc(s: &str) -> Result<DateTime<Utc>> {
    let utc = DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|e| Error::InvalidDateTime(format!("{s}: {e}")))?;
    check_year(utc.naive_utc().date())?;
    Ok(utc)
}

fn truncate_naive(dt: NaiveDateTime) -> NaiveDateTime {
    dt.with_nanosecond(0).unwrap_or(dt)
}

fn truncate_utc(dt: DateTime<Utc>) -> DateTime<Utc> {
    dt.with_nanosecond(0).unwrap_or(dt)
}

impl<'de> Deserialize<'de> for EventTime {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Helper {
            utc: Option<String>,
            localtime: Option<String>,
            timezone: Option<String>,
            date: Option<NaiveDate>,
        }

        let Helper {
            utc,
            localtime,
            timezone,
            date,
        } = Helper::deserialize(deserializer)?;
        match (utc, localtime, timezone, date) {
            (None, None, None, Some(date)) => Self::try_all_day(date).map_err(de::Error::custom),
            (Some(utc), Some(localtime), Some(timezone), None) => Ok(Self::Timed {
                utc: parse_utc(&utc).map_err(de::Error::custom)?,
                localtime: parse_localtime(&localtime).map_err(de::Error::custom)?,
                timezone: parse_timezone(&timezone).map_err(de::Error::custom)?,
            }),
            (_, _, _, Some(_)) => Err(de::Error::custom(
                "event time must contain either `date` or `utc`/`localtime`/`timezone`, not both",
            )),
            _ => Err(de::Error::custom(
                "event time must contain either `date` or all of `utc`, `localtime` and `timezone`",
            )),
        }
    }
}

fn parse_localtime(s: &str) -> Result<NaiveDateTime> {
    let local = s
        .parse::<NaiveDateTime>()
        .map_err(|e| Error::InvalidDateTime(format!("{s}: {e}")))?;
    check_year(local.date())?;
    Ok(local)
}

impl fmt::Display for EventTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Timed {
                localtime,
                timezone,
                ..
            } => {
                write!(
                    f,
                    "{} ({})",
                    localtime.format("%Y-%m-%d %H:%M"),
                    timezone.name()
                )
            }
            Self::AllDay { date } => write!(f, "{}", date.format(DATE_FORMAT)),
        }
    }
}

/// Serde helpers for UTC timestamps in the MSC wire format.
///
/// Serializes as `YYYY-MM-DDTHH:MM:SSZ` (no fractional seconds) and accepts any RFC 3339
/// timestamp on input. Use with `#[serde(with = "utc_serde")]`, or the [`option`](utc_serde::option)
/// / [`vec`](utc_serde::vec) submodules.
pub mod utc_serde {
    use chrono::{DateTime, Utc};
    use serde::{Deserialize, Deserializer, Serializer, de};

    use super::{format_utc, parse_utc};

    /// Serializes a UTC instant.
    pub fn serialize<S: Serializer>(dt: &DateTime<Utc>, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&format_utc(dt))
    }

    /// Deserializes an RFC 3339 timestamp into UTC.
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<DateTime<Utc>, D::Error> {
        let s = String::deserialize(d)?;
        parse_utc(&s).map_err(de::Error::custom)
    }

    /// Serde helpers for `Option<DateTime<Utc>>`.
    pub mod option {
        use chrono::{DateTime, Utc};
        use serde::{Deserialize, Deserializer, Serializer, de};

        use super::super::{format_utc, parse_utc};

        /// Serializes an optional UTC instant.
        pub fn serialize<S: Serializer>(
            dt: &Option<DateTime<Utc>>,
            s: S,
        ) -> Result<S::Ok, S::Error> {
            match dt {
                Some(dt) => s.serialize_str(&format_utc(dt)),
                None => s.serialize_none(),
            }
        }

        /// Deserializes an optional RFC 3339 timestamp into UTC.
        pub fn deserialize<'de, D: Deserializer<'de>>(
            d: D,
        ) -> Result<Option<DateTime<Utc>>, D::Error> {
            Option::<String>::deserialize(d)?
                .map(|s| parse_utc(&s).map_err(de::Error::custom))
                .transpose()
        }
    }

    /// Serde helpers for `Vec<DateTime<Utc>>`.
    pub mod vec {
        use chrono::{DateTime, Utc};
        use serde::{Deserialize, Deserializer, Serializer, de, ser::SerializeSeq};

        use super::super::{format_utc, parse_utc};

        /// Serializes a list of UTC instants.
        pub fn serialize<S: Serializer>(dts: &[DateTime<Utc>], s: S) -> Result<S::Ok, S::Error> {
            let mut seq = s.serialize_seq(Some(dts.len()))?;
            for dt in dts {
                seq.serialize_element(&format_utc(dt))?;
            }
            seq.end()
        }

        /// Deserializes a list of RFC 3339 timestamps into UTC.
        pub fn deserialize<'de, D: Deserializer<'de>>(
            d: D,
        ) -> Result<Vec<DateTime<Utc>>, D::Error> {
            Vec::<String>::deserialize(d)?
                .iter()
                .map(|s| parse_utc(s).map_err(de::Error::custom))
                .collect()
        }
    }
}

fn serialize_localtime<S: serde::Serializer>(
    dt: &NaiveDateTime,
    s: S,
) -> std::result::Result<S::Ok, S::Error> {
    s.serialize_str(&dt.format(LOCALTIME_FORMAT).to_string())
}

#[cfg(test)]
mod tests {
    use chrono_tz::Europe::Berlin;
    use serde_json::json;

    use super::*;

    fn naive(s: &str) -> NaiveDateTime {
        s.parse().unwrap()
    }

    #[test]
    fn timed_serializes_to_msc_shape() {
        let t = EventTime::timed(naive("2026-07-01T09:00:00"), Berlin).unwrap();
        assert_eq!(
            serde_json::to_value(t).unwrap(),
            json!({
                "utc": "2026-07-01T07:00:00Z",
                "localtime": "2026-07-01T09:00:00",
                "timezone": "Europe/Berlin"
            })
        );
        t.validate().unwrap();
    }

    #[test]
    fn all_day_round_trip() {
        let v = json!({ "date": "2026-12-25" });
        let t: EventTime = serde_json::from_value(v.clone()).unwrap();
        assert_eq!(
            t,
            EventTime::all_day(NaiveDate::from_ymd_opt(2026, 12, 25).unwrap())
        );
        assert!(t.is_all_day());
        assert_eq!(serde_json::to_value(t).unwrap(), v);
    }

    #[test]
    fn deserialize_accepts_rfc3339_offsets() {
        let t: EventTime = serde_json::from_value(json!({
            "utc": "2026-07-01T09:00:00.000+02:00",
            "localtime": "2026-07-01T09:00:00",
            "timezone": "Europe/Berlin"
        }))
        .unwrap();
        t.validate().unwrap();
        assert_eq!(
            serde_json::to_value(t).unwrap()["utc"],
            "2026-07-01T07:00:00Z"
        );
    }

    #[test]
    fn deserialize_rejects_unknown_timezone_and_mixed_shapes() {
        assert!(
            serde_json::from_value::<EventTime>(json!({
                "utc": "2026-07-01T07:00:00Z",
                "localtime": "2026-07-01T09:00:00",
                "timezone": "Mars/Olympus_Mons"
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<EventTime>(json!({
                "utc": "2026-07-01T07:00:00Z",
                "date": "2026-07-01"
            }))
            .is_err()
        );
        assert!(serde_json::from_value::<EventTime>(json!({})).is_err());
    }

    #[test]
    fn validate_detects_inconsistency() {
        let t = EventTime::Timed {
            utc: parse_utc("2026-07-01T07:00:00Z").unwrap(),
            localtime: naive("2026-07-01T08:00:00"),
            timezone: Berlin,
        };
        assert!(matches!(t.validate(), Err(Error::InconsistentDateTime(_))));
    }

    #[test]
    fn spring_forward_gap_is_an_error() {
        // 2026-03-29 02:30 does not exist in Europe/Berlin (clocks jump 02:00 -> 03:00).
        let err = EventTime::timed(naive("2026-03-29T02:30:00"), Berlin).unwrap_err();
        assert!(matches!(err, Error::InvalidDateTime(_)));
    }

    #[test]
    fn autumn_ambiguity_picks_earlier_instant() {
        // 2026-10-25 02:30 occurs twice in Europe/Berlin: first CEST (+02:00), then CET (+01:00).
        let t = EventTime::timed(naive("2026-10-25T02:30:00"), Berlin).unwrap();
        assert_eq!(t.to_utc(Berlin), parse_utc("2026-10-25T00:30:00Z").unwrap());
        t.validate().unwrap();

        // The later instant is still representable via from_utc and validates.
        let later = EventTime::from_utc(parse_utc("2026-10-25T01:30:00Z").unwrap(), Berlin);
        assert_eq!(
            later.local_date(),
            NaiveDate::from_ymd_opt(2026, 10, 25).unwrap()
        );
        later.validate().unwrap();
    }

    #[test]
    fn from_utc_computes_localtime_and_truncates() {
        let utc = parse_utc("2026-01-15T12:34:56.789Z").unwrap();
        let t = EventTime::from_utc(utc, Berlin);
        assert_eq!(
            serde_json::to_value(t).unwrap(),
            json!({
                "utc": "2026-01-15T12:34:56Z",
                "localtime": "2026-01-15T13:34:56",
                "timezone": "Europe/Berlin"
            })
        );
    }

    #[test]
    fn all_day_to_utc_uses_fallback_midnight() {
        let t = EventTime::all_day(NaiveDate::from_ymd_opt(2026, 12, 25).unwrap());
        assert_eq!(t.to_utc(Berlin), parse_utc("2026-12-24T23:00:00Z").unwrap());
        assert_eq!(
            t.to_utc(chrono_tz::UTC),
            parse_utc("2026-12-25T00:00:00Z").unwrap()
        );
    }

    #[test]
    fn lenient_resolution_of_gap() {
        let utc = resolve_local_lenient(naive("2026-03-29T02:30:00"), Berlin);
        // Interpreted with the pre-gap offset (+01:00): 01:30Z, i.e. 03:30 CEST.
        assert_eq!(utc, parse_utc("2026-03-29T01:30:00Z").unwrap());
    }

    #[test]
    fn parse_timezone_validates() {
        assert_eq!(parse_timezone("Europe/Berlin").unwrap(), Berlin);
        assert!(matches!(
            parse_timezone("+02:00"),
            Err(Error::InvalidTimezone(_))
        ));
    }
}
