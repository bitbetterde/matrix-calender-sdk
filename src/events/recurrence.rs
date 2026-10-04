//! Recurrence rule types (MSC4496 §2.5).
//!
//! These are serde types only: the RFC 5545 §3.3.10 `RRULE` grammar expressed as a structured
//! JSON object, plus conversion to and from the textual RFC 5545 form. Expansion into concrete
//! occurrences lives in [`crate::recurrence`].

use std::{collections::HashSet, fmt, str::FromStr};

use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

use super::datetime::utc_serde;
use crate::{Error, Result};

/// A recurrence: a rule plus explicit exclusions and additions.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Recurrence {
    /// The recurrence rule.
    pub rrule: RRule,
    /// Original UTC start times of excluded occurrences (iCalendar `EXDATE`).
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        with = "utc_serde::vec"
    )]
    pub exdates: Vec<DateTime<Utc>>,
    /// UTC start times of additional occurrences (iCalendar `RDATE`).
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        with = "utc_serde::vec"
    )]
    pub rdates: Vec<DateTime<Utc>>,
}

impl Recurrence {
    /// A recurrence with the given rule and no exdates/rdates.
    pub fn new(rrule: RRule) -> Self {
        Self {
            rrule,
            exdates: Vec::new(),
            rdates: Vec::new(),
        }
    }
}

string_enum! {
    /// Recurrence frequency (`FREQ`).
    ///
    /// Unknown values deserialize into [`Frequency::Other`] so that the event round-trips, but
    /// such a rule fails [`RRule::validate`] and cannot be expanded.
    #[derive(PartialOrd, Ord)]
    pub enum Frequency {
        /// `SECONDLY`
        Secondly = "SECONDLY",
        /// `MINUTELY`
        Minutely = "MINUTELY",
        /// `HOURLY`
        Hourly = "HOURLY",
        /// `DAILY`
        Daily = "DAILY",
        /// `WEEKLY`
        Weekly = "WEEKLY",
        /// `MONTHLY`
        Monthly = "MONTHLY",
        /// `YEARLY`
        Yearly = "YEARLY",
    }
}

impl FromStr for Frequency {
    type Err = Error;

    /// Parses an RFC 5545 `FREQ` value case-insensitively; unknown values are an error.
    fn from_str(s: &str) -> Result<Self> {
        match Self::from(s.to_ascii_uppercase().as_str()) {
            Self::Other(_) => Err(Error::InvalidRecurrence(format!("unknown FREQ {s}"))),
            freq => Ok(freq),
        }
    }
}

/// A day of the week, serialized as the two-letter RFC 5545 code (`MO` … `SU`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Weekday {
    /// Monday
    #[serde(rename = "MO")]
    Monday,
    /// Tuesday
    #[serde(rename = "TU")]
    Tuesday,
    /// Wednesday
    #[serde(rename = "WE")]
    Wednesday,
    /// Thursday
    #[serde(rename = "TH")]
    Thursday,
    /// Friday
    #[serde(rename = "FR")]
    Friday,
    /// Saturday
    #[serde(rename = "SA")]
    Saturday,
    /// Sunday
    #[serde(rename = "SU")]
    Sunday,
}

impl Weekday {
    /// The two-letter RFC 5545 code.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Monday => "MO",
            Self::Tuesday => "TU",
            Self::Wednesday => "WE",
            Self::Thursday => "TH",
            Self::Friday => "FR",
            Self::Saturday => "SA",
            Self::Sunday => "SU",
        }
    }
}

impl FromStr for Weekday {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        Ok(match s.to_ascii_uppercase().as_str() {
            "MO" => Self::Monday,
            "TU" => Self::Tuesday,
            "WE" => Self::Wednesday,
            "TH" => Self::Thursday,
            "FR" => Self::Friday,
            "SA" => Self::Saturday,
            "SU" => Self::Sunday,
            _ => return Err(Error::InvalidRecurrence(format!("unknown weekday {s}"))),
        })
    }
}

impl fmt::Display for Weekday {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl From<Weekday> for chrono::Weekday {
    fn from(w: Weekday) -> Self {
        match w {
            Weekday::Monday => Self::Mon,
            Weekday::Tuesday => Self::Tue,
            Weekday::Wednesday => Self::Wed,
            Weekday::Thursday => Self::Thu,
            Weekday::Friday => Self::Fri,
            Weekday::Saturday => Self::Sat,
            Weekday::Sunday => Self::Sun,
        }
    }
}

impl From<chrono::Weekday> for Weekday {
    fn from(w: chrono::Weekday) -> Self {
        match w {
            chrono::Weekday::Mon => Self::Monday,
            chrono::Weekday::Tue => Self::Tuesday,
            chrono::Weekday::Wed => Self::Wednesday,
            chrono::Weekday::Thu => Self::Thursday,
            chrono::Weekday::Fri => Self::Friday,
            chrono::Weekday::Sat => Self::Saturday,
            chrono::Weekday::Sun => Self::Sunday,
        }
    }
}

/// A `BYDAY` entry: a weekday with an optional ordinal, e.g. `MO`, `-1FR`, `2TU`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ByDay {
    /// The occurrence within the month or year (`1`..=`53` or `-53`..=`-1`), if any.
    pub ordinal: Option<i8>,
    /// The weekday.
    pub weekday: Weekday,
}

impl ByDay {
    /// Every `weekday` in the period.
    pub fn every(weekday: Weekday) -> Self {
        Self {
            ordinal: None,
            weekday,
        }
    }

    /// The `ordinal`-th `weekday` in the period (negative counts from the end).
    pub fn nth(ordinal: i8, weekday: Weekday) -> Self {
        Self {
            ordinal: Some(ordinal),
            weekday,
        }
    }
}

impl From<Weekday> for ByDay {
    fn from(weekday: Weekday) -> Self {
        Self::every(weekday)
    }
}

impl fmt::Display for ByDay {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(n) = self.ordinal {
            write!(f, "{n}")?;
        }
        f.write_str(self.weekday.as_str())
    }
}

impl FromStr for ByDay {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        let s = s.trim();
        if s.len() < 2 || !s.is_char_boundary(s.len() - 2) {
            return Err(Error::InvalidRecurrence(format!("invalid BYDAY value {s}")));
        }
        let (num, day) = s.split_at(s.len() - 2);
        let weekday = day.parse()?;
        let ordinal = if num.is_empty() {
            None
        } else {
            let n: i8 = num
                .parse()
                .map_err(|_| Error::InvalidRecurrence(format!("invalid BYDAY ordinal {s}")))?;
            if n == 0 || !(-53..=53).contains(&n) {
                return Err(Error::InvalidRecurrence(format!(
                    "BYDAY ordinal out of range: {s}"
                )));
            }
            Some(n)
        };
        Ok(Self { ordinal, weekday })
    }
}

impl Serialize for ByDay {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for ByDay {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(de::Error::custom)
    }
}

/// The `UNTIL` bound: either a date (as in the MSC example) or a UTC date-time.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Until {
    /// A date, serialized as `YYYY-MM-DD`; the rule ends after this (inclusive) local date.
    Date(NaiveDate),
    /// A UTC date-time, serialized as `YYYY-MM-DDTHH:MM:SSZ`.
    DateTime(#[serde(with = "utc_serde")] DateTime<Utc>),
}

impl Until {
    /// The RFC 5545 textual form (`YYYYMMDD` or `YYYYMMDDTHHMMSSZ`).
    pub fn to_rfc5545_string(&self) -> String {
        match self {
            Self::Date(d) => d.format("%Y%m%d").to_string(),
            Self::DateTime(dt) => dt.format("%Y%m%dT%H%M%SZ").to_string(),
        }
    }

    /// Parses the RFC 5545 textual form (`YYYYMMDD` or `YYYYMMDDTHHMMSSZ`).
    ///
    /// Floating (non-UTC) date-times are rejected because they cannot be represented in the
    /// MSC's JSON form.
    pub fn parse_rfc5545(s: &str) -> Result<Self> {
        let err = || Error::InvalidRecurrence(format!("invalid UNTIL value {s}"));
        if s.len() == 8 {
            NaiveDate::parse_from_str(s, "%Y%m%d")
                .map(Self::Date)
                .map_err(|_| err())
        } else if let Some(naive) = s.strip_suffix('Z').or_else(|| s.strip_suffix('z')) {
            NaiveDateTime::parse_from_str(naive, "%Y%m%dT%H%M%S")
                .map(|dt| Self::DateTime(dt.and_utc()))
                .map_err(|_| err())
        } else {
            Err(err())
        }
    }
}

/// A recurrence rule mirroring RFC 5545 §3.3.10 as a JSON object.
///
/// All optional parts are omitted from the JSON when `None`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RRule {
    /// `FREQ`
    pub freq: Frequency,
    /// `INTERVAL` (defaults to 1 when absent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval: Option<u32>,
    /// `COUNT`; mutually exclusive with `until`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<u32>,
    /// `UNTIL`; mutually exclusive with `count`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<Until>,
    /// `BYSECOND` (0..=60).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bysecond: Option<Vec<u32>>,
    /// `BYMINUTE` (0..=59).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub byminute: Option<Vec<u32>>,
    /// `BYHOUR` (0..=23).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub byhour: Option<Vec<u32>>,
    /// `BYDAY`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub byday: Option<Vec<ByDay>>,
    /// `BYMONTHDAY` (±1..=31).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bymonthday: Option<Vec<i32>>,
    /// `BYYEARDAY` (±1..=366).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub byyearday: Option<Vec<i32>>,
    /// `BYWEEKNO` (±1..=53).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub byweekno: Option<Vec<i32>>,
    /// `BYMONTH` (1..=12).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bymonth: Option<Vec<u32>>,
    /// `BYSETPOS` (±1..=366).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bysetpos: Option<Vec<i32>>,
    /// `WKST`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wkst: Option<Weekday>,
}

impl RRule {
    /// A rule with the given frequency and no other parts.
    pub fn new(freq: Frequency) -> Self {
        Self {
            freq,
            interval: None,
            count: None,
            until: None,
            bysecond: None,
            byminute: None,
            byhour: None,
            byday: None,
            bymonthday: None,
            byyearday: None,
            byweekno: None,
            bymonth: None,
            bysetpos: None,
            wkst: None,
        }
    }

    /// Checks the constraints of RFC 5545 §3.3.10 that can be checked without a start date:
    /// `COUNT`/`UNTIL` exclusivity, value ranges, and which `BY*` parts are allowed for the
    /// frequency.
    pub fn validate(&self) -> Result<()> {
        let err = |m: String| Err(Error::InvalidRecurrence(m));
        if self.count.is_some() && self.until.is_some() {
            return err("COUNT and UNTIL are mutually exclusive".into());
        }
        if let Frequency::Other(freq) = &self.freq {
            return err(format!("unsupported FREQ {freq}"));
        }
        if self.interval == Some(0) {
            return err("INTERVAL must be at least 1".into());
        }
        check_range("BYSECOND", self.bysecond.as_deref(), 0, 60)?;
        check_range("BYMINUTE", self.byminute.as_deref(), 0, 59)?;
        check_range("BYHOUR", self.byhour.as_deref(), 0, 23)?;
        check_range("BYMONTH", self.bymonth.as_deref(), 1, 12)?;
        check_signed("BYMONTHDAY", self.bymonthday.as_deref(), 31)?;
        check_signed("BYYEARDAY", self.byyearday.as_deref(), 366)?;
        check_signed("BYWEEKNO", self.byweekno.as_deref(), 53)?;
        check_signed("BYSETPOS", self.bysetpos.as_deref(), 366)?;

        use Frequency::*;
        if self.bymonthday.is_some() && self.freq == Weekly {
            return err("BYMONTHDAY is not allowed with FREQ=WEEKLY".into());
        }
        if self.byyearday.is_some() && matches!(self.freq, Daily | Weekly | Monthly) {
            return err(format!("BYYEARDAY is not allowed with FREQ={}", self.freq));
        }
        if self.byweekno.is_some() && self.freq != Yearly {
            return err("BYWEEKNO is only allowed with FREQ=YEARLY".into());
        }
        if let Some(byday) = &self.byday
            && byday.iter().any(|d| d.ordinal.is_some())
            && (!matches!(self.freq, Monthly | Yearly)
                || (self.freq == Yearly && self.byweekno.is_some()))
        {
            return err(
                "BYDAY ordinals are only allowed with FREQ=MONTHLY or YEARLY (without BYWEEKNO)"
                    .into(),
            );
        }
        if self.bysetpos.is_some()
            && self.bysecond.is_none()
            && self.byminute.is_none()
            && self.byhour.is_none()
            && self.byday.is_none()
            && self.bymonthday.is_none()
            && self.byyearday.is_none()
            && self.byweekno.is_none()
            && self.bymonth.is_none()
        {
            return err("BYSETPOS requires another BY* rule part".into());
        }
        Ok(())
    }

    /// Formats the rule as an RFC 5545 `RRULE` value (without the `RRULE:` prefix), e.g.
    /// `FREQ=WEEKLY;BYDAY=MO;UNTIL=20261231`.
    ///
    /// Parts are emitted in a fixed order: `FREQ`, `INTERVAL`, `BYMONTH`, `BYWEEKNO`,
    /// `BYYEARDAY`, `BYMONTHDAY`, `BYDAY`, `BYHOUR`, `BYMINUTE`, `BYSECOND`, `BYSETPOS`,
    /// `WKST`, `COUNT`, `UNTIL`.
    pub fn to_rfc5545_string(&self) -> String {
        fn list<T: fmt::Display>(v: &[T]) -> String {
            v.iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(",")
        }

        let mut parts = vec![format!("FREQ={}", self.freq)];
        if let Some(i) = self.interval {
            parts.push(format!("INTERVAL={i}"));
        }
        let lists: [(&str, Option<String>); 10] = [
            ("BYMONTH", self.bymonth.as_deref().map(list)),
            ("BYWEEKNO", self.byweekno.as_deref().map(list)),
            ("BYYEARDAY", self.byyearday.as_deref().map(list)),
            ("BYMONTHDAY", self.bymonthday.as_deref().map(list)),
            ("BYDAY", self.byday.as_deref().map(list)),
            ("BYHOUR", self.byhour.as_deref().map(list)),
            ("BYMINUTE", self.byminute.as_deref().map(list)),
            ("BYSECOND", self.bysecond.as_deref().map(list)),
            ("BYSETPOS", self.bysetpos.as_deref().map(list)),
            ("WKST", self.wkst.map(|w| w.to_string())),
        ];
        parts.extend(
            lists
                .into_iter()
                .filter_map(|(k, v)| v.map(|v| format!("{k}={v}"))),
        );
        if let Some(c) = self.count {
            parts.push(format!("COUNT={c}"));
        }
        if let Some(u) = &self.until {
            parts.push(format!("UNTIL={}", u.to_rfc5545_string()));
        }
        parts.join(";")
    }

    /// Parses an RFC 5545 `RRULE` value. An optional `RRULE:` prefix is accepted, keys are
    /// case-insensitive, and the result is checked with [`RRule::validate`].
    pub fn parse_rfc5545(s: &str) -> Result<Self> {
        let s = s.trim();
        let s = match s.get(..6) {
            Some(prefix) if prefix.eq_ignore_ascii_case("RRULE:") => &s[6..],
            _ => s,
        };

        let mut freq = None;
        let mut rule = Self::new(Frequency::Daily);
        let mut seen = HashSet::new();

        for part in s.split(';').filter(|p| !p.trim().is_empty()) {
            let (key, value) = part
                .split_once('=')
                .ok_or_else(|| Error::InvalidRecurrence(format!("missing '=' in {part}")))?;
            let key = key.trim().to_ascii_uppercase();
            let value = value.trim();
            if !seen.insert(key.clone()) {
                return Err(Error::InvalidRecurrence(format!(
                    "duplicate rule part {key}"
                )));
            }
            match key.as_str() {
                "FREQ" => freq = Some(value.parse()?),
                "INTERVAL" => rule.interval = Some(parse_num(&key, value)?),
                "COUNT" => rule.count = Some(parse_num(&key, value)?),
                "UNTIL" => rule.until = Some(Until::parse_rfc5545(value)?),
                "BYSECOND" => rule.bysecond = Some(parse_list(&key, value)?),
                "BYMINUTE" => rule.byminute = Some(parse_list(&key, value)?),
                "BYHOUR" => rule.byhour = Some(parse_list(&key, value)?),
                "BYDAY" => {
                    rule.byday = Some(
                        value
                            .split(',')
                            .map(str::parse)
                            .collect::<Result<Vec<ByDay>>>()?,
                    )
                }
                "BYMONTHDAY" => rule.bymonthday = Some(parse_list(&key, value)?),
                "BYYEARDAY" => rule.byyearday = Some(parse_list(&key, value)?),
                "BYWEEKNO" => rule.byweekno = Some(parse_list(&key, value)?),
                "BYMONTH" => rule.bymonth = Some(parse_list(&key, value)?),
                "BYSETPOS" => rule.bysetpos = Some(parse_list(&key, value)?),
                "WKST" => rule.wkst = Some(value.parse()?),
                _ => return Err(Error::InvalidRecurrence(format!("unknown rule part {key}"))),
            }
        }

        rule.freq = freq.ok_or_else(|| Error::InvalidRecurrence("missing FREQ".into()))?;
        rule.validate()?;
        Ok(rule)
    }
}

impl fmt::Display for RRule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_rfc5545_string())
    }
}

impl FromStr for RRule {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        Self::parse_rfc5545(s)
    }
}

fn parse_num<T: FromStr>(key: &str, value: &str) -> Result<T> {
    value
        .parse()
        .map_err(|_| Error::InvalidRecurrence(format!("invalid {key} value {value}")))
}

fn parse_list<T: FromStr>(key: &str, value: &str) -> Result<Vec<T>> {
    value.split(',').map(|v| parse_num(key, v.trim())).collect()
}

fn check_range(key: &str, values: Option<&[u32]>, min: u32, max: u32) -> Result<()> {
    match values.and_then(|v| v.iter().find(|n| !(min..=max).contains(*n))) {
        Some(n) => Err(Error::InvalidRecurrence(format!(
            "{key} value {n} out of range"
        ))),
        None => Ok(()),
    }
}

fn check_signed(key: &str, values: Option<&[i32]>, max: i32) -> Result<()> {
    match values.and_then(|v| v.iter().find(|n| **n == 0 || n.abs() > max)) {
        Some(n) => Err(Error::InvalidRecurrence(format!(
            "{key} value {n} out of range"
        ))),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn msc_example() {
        let v = json!({
          "rrule": {
            "freq": "WEEKLY",
            "byday": ["MO"],
            "until": "2026-12-31"
          },
          "exdates": ["2026-08-05T07:00:00Z"]
        });
        let r: Recurrence = serde_json::from_value(v.clone()).unwrap();
        assert_eq!(r.rrule.freq, Frequency::Weekly);
        assert_eq!(r.rrule.byday, Some(vec![ByDay::every(Weekday::Monday)]));
        assert_eq!(
            r.rrule.until,
            Some(Until::Date(NaiveDate::from_ymd_opt(2026, 12, 31).unwrap()))
        );
        assert_eq!(
            r.rrule.to_rfc5545_string(),
            "FREQ=WEEKLY;BYDAY=MO;UNTIL=20261231"
        );

        let mut built = Recurrence::new(RRule {
            byday: Some(vec![Weekday::Monday.into()]),
            until: Some(Until::Date(NaiveDate::from_ymd_opt(2026, 12, 31).unwrap())),
            ..RRule::new(Frequency::Weekly)
        });
        built.exdates.push("2026-08-05T07:00:00Z".parse().unwrap());
        assert_eq!(built, r);
        assert_eq!(serde_json::to_value(&built).unwrap(), v);
    }

    #[test]
    fn unknown_freq_round_trips_but_does_not_validate() {
        let v = json!({ "freq": "FORTNIGHTLY", "byday": ["MO"] });
        let r: RRule = serde_json::from_value(v.clone()).unwrap();
        assert_eq!(r.freq, Frequency::Other("FORTNIGHTLY".into()));
        assert_eq!(serde_json::to_value(&r).unwrap(), v);
        assert!(matches!(r.validate(), Err(Error::InvalidRecurrence(_))));
        assert_eq!("weekly".parse::<Frequency>().unwrap(), Frequency::Weekly);
        assert!("fortnightly".parse::<Frequency>().is_err());
    }

    #[test]
    fn until_datetime_json() {
        let v = json!({ "freq": "DAILY", "count": null, "until": "2026-12-31T23:00:00Z" });
        let r: RRule = serde_json::from_value(v).unwrap();
        assert!(matches!(r.until, Some(Until::DateTime(_))));
        assert_eq!(
            serde_json::to_value(&r).unwrap(),
            json!({ "freq": "DAILY", "until": "2026-12-31T23:00:00Z" })
        );
        assert_eq!(r.to_rfc5545_string(), "FREQ=DAILY;UNTIL=20261231T230000Z");
    }

    #[test]
    fn byday_strings() {
        for (s, expected) in [
            ("MO", ByDay::every(Weekday::Monday)),
            ("-1FR", ByDay::nth(-1, Weekday::Friday)),
            ("2TU", ByDay::nth(2, Weekday::Tuesday)),
            ("+3WE", ByDay::nth(3, Weekday::Wednesday)),
        ] {
            assert_eq!(s.parse::<ByDay>().unwrap(), expected);
        }
        assert_eq!(ByDay::nth(-1, Weekday::Friday).to_string(), "-1FR");
        assert_eq!(
            serde_json::to_value(ByDay::nth(2, Weekday::Tuesday)).unwrap(),
            json!("2TU")
        );
        assert!("0MO".parse::<ByDay>().is_err());
        assert!("54MO".parse::<ByDay>().is_err());
        assert!("XX".parse::<ByDay>().is_err());
        assert!("M".parse::<ByDay>().is_err());
        assert!("1ü".parse::<ByDay>().is_err());
    }

    #[test]
    fn rfc5545_round_trips() {
        for s in [
            "FREQ=WEEKLY;BYDAY=MO;UNTIL=20261231",
            "FREQ=DAILY;COUNT=10",
            "FREQ=WEEKLY;INTERVAL=2;BYDAY=MO,WE,FR;WKST=SU;UNTIL=20261231T225959Z",
            "FREQ=MONTHLY;BYDAY=-1FR",
            "FREQ=MONTHLY;BYDAY=MO,TU,WE,TH,FR;BYSETPOS=-1",
            "FREQ=YEARLY;BYMONTH=1;BYDAY=SU,MO,TU,WE,TH,FR,SA",
            "FREQ=YEARLY;BYWEEKNO=20;BYDAY=MO",
            "FREQ=YEARLY;BYYEARDAY=1,100,200",
            "FREQ=MONTHLY;BYMONTHDAY=-3,15",
            "FREQ=DAILY;BYHOUR=9,10,11;BYMINUTE=0,20,40;BYSECOND=0",
        ] {
            let rule = RRule::parse_rfc5545(s).unwrap();
            assert_eq!(rule.to_rfc5545_string(), s);
            // And through JSON.
            let json = serde_json::to_value(&rule).unwrap();
            let back: RRule = serde_json::from_value(json).unwrap();
            assert_eq!(back, rule);
        }
    }

    #[test]
    fn parse_is_lenient_about_prefix_order_and_case() {
        let rule = RRule::parse_rfc5545("RRULE:until=20261231;byday=mo;freq=weekly").unwrap();
        assert_eq!(
            rule.to_rfc5545_string(),
            "FREQ=WEEKLY;BYDAY=MO;UNTIL=20261231"
        );
    }

    #[test]
    fn parse_rejects_invalid_rules() {
        for s in [
            "",
            "BYDAY=MO",
            "FREQ=FORTNIGHTLY",
            "FREQ=DAILY;FREQ=WEEKLY",
            "FREQ=DAILY;COUNT=3;UNTIL=20261231",
            "FREQ=DAILY;INTERVAL=0",
            "FREQ=DAILY;BYHOUR=24",
            "FREQ=WEEKLY;BYMONTHDAY=1",
            "FREQ=WEEKLY;BYDAY=1MO",
            "FREQ=MONTHLY;BYWEEKNO=1",
            "FREQ=DAILY;BYSETPOS=1",
            "FREQ=DAILY;UNTIL=20261231T120000",
            "FREQ=DAILY;FOO=1",
            "FREQ=DAILY;COUNT",
        ] {
            assert!(RRule::parse_rfc5545(s).is_err(), "{s} should be rejected");
        }
    }
}
