//! Availability state events and API payloads (MSC4496 §4).

use chrono::{DateTime, NaiveTime, Timelike, Utc};
use chrono_tz::Tz;
use matrix_sdk::ruma::{
    OwnedUserId,
    events::{EmptyStateKey, macros::EventContent},
};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

use super::{datetime::utc_serde, recurrence::Weekday};

/// Tier 0: no availability is shared (queries are rejected with `M_FORBIDDEN`).
pub const TIER_NONE: u8 = 0;
/// Tier 1: busy/free intervals only.
pub const TIER_BUSY: u8 = 1;
/// Tier 2: busy intervals with event titles.
pub const TIER_TITLES: u8 = 2;

/// Content of the `org.matrix.msc4496.availability_policy` (stable:
/// `m.calendar.availability_policy`) state event, sent with an empty state key into the
/// user's calendar room.
///
/// Unlike the other calendar events this has no `m.text` or nested content block.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, EventContent)]
#[ruma_event(
    type = "org.matrix.msc4496.availability_policy",
    alias = "m.calendar.availability_policy",
    kind = State,
    state_key_type = EmptyStateKey
)]
pub struct AvailabilityPolicyEventContent {
    /// The tier for callers without a matching grant. Defaults to [`TIER_NONE`].
    #[serde(default)]
    pub default_tier: u8,
    /// Explicit grants.
    #[serde(default)]
    pub grants: Vec<Grant>,
}

impl AvailabilityPolicyEventContent {
    /// Creates a policy.
    pub fn new(default_tier: u8, grants: Vec<Grant>) -> Self {
        Self {
            default_tier,
            grants,
        }
    }
}

/// A grant of an availability tier to a subject.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grant {
    /// A Matrix user ID (`@bob:example.com`) or a server wildcard (`*:trusted-corp.example`).
    pub subject: String,
    /// The granted tier.
    pub tier: u8,
}

impl Grant {
    /// Creates a grant.
    pub fn new(subject: impl Into<String>, tier: u8) -> Self {
        Self {
            subject: subject.into(),
            tier,
        }
    }
}

/// Content of the `org.matrix.msc4496.availability_window` (stable:
/// `m.calendar.availability_window`) state event: the user's general working hours, sent with
/// an empty state key.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, EventContent)]
#[ruma_event(
    type = "org.matrix.msc4496.availability_window",
    alias = "m.calendar.availability_window",
    kind = State,
    state_key_type = EmptyStateKey
)]
pub struct AvailabilityWindowEventContent {
    /// The working-hours windows.
    #[serde(default)]
    pub windows: Vec<Window>,
}

impl AvailabilityWindowEventContent {
    /// Creates the content.
    pub fn new(windows: Vec<Window>) -> Self {
        Self { windows }
    }
}

/// A weekly recurring window of availability.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Window {
    /// Days of the week the window applies to.
    pub days: Vec<Weekday>,
    /// Local start time, serialized as `HH:MM`.
    #[serde(with = "hhmm")]
    pub start_time: NaiveTime,
    /// Local end time, serialized as `HH:MM`.
    #[serde(with = "hhmm")]
    pub end_time: NaiveTime,
    /// IANA timezone of `start_time`/`end_time`.
    pub timezone: Tz,
}

mod hhmm {
    use super::*;

    pub(super) fn serialize<S: Serializer>(t: &NaiveTime, s: S) -> Result<S::Ok, S::Error> {
        let fmt = if t.second() == 0 { "%H:%M" } else { "%H:%M:%S" };
        s.collect_str(&t.format(fmt))
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<NaiveTime, D::Error> {
        let s = String::deserialize(d)?;
        NaiveTime::parse_from_str(&s, "%H:%M")
            .or_else(|_| NaiveTime::parse_from_str(&s, "%H:%M:%S"))
            .map_err(|e| de::Error::custom(format!("invalid time {s}: {e}")))
    }
}

/// Request body of the availability query endpoint (§4.5).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AvailabilityRequest {
    /// Start of the queried range.
    #[serde(with = "utc_serde")]
    pub range_start: DateTime<Utc>,
    /// End of the queried range.
    #[serde(with = "utc_serde")]
    pub range_end: DateTime<Utc>,
}

/// Response body of the availability query endpoint (§4.5).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AvailabilityResponse {
    /// The queried user.
    pub user_id: OwnedUserId,
    /// Start of the queried range.
    #[serde(with = "utc_serde")]
    pub range_start: DateTime<Utc>,
    /// End of the queried range.
    #[serde(with = "utc_serde")]
    pub range_end: DateTime<Utc>,
    /// Busy intervals within the range.
    #[serde(default)]
    pub busy: Vec<BusyInterval>,
}

/// A busy interval.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BusyInterval {
    /// Interval start.
    #[serde(with = "utc_serde")]
    pub start: DateTime<Utc>,
    /// Interval end.
    #[serde(with = "utc_serde")]
    pub end: DateTime<Utc>,
    /// The busy type.
    #[serde(rename = "type")]
    pub kind: BusyType,
    /// The event title, only present for Tier 2 callers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

string_enum! {
    /// iCalendar FREEBUSY type (RFC 5545 §3.2.9).
    pub enum BusyType {
        /// `BUSY`
        Busy = "BUSY",
        /// `BUSY-UNAVAILABLE`
        BusyUnavailable = "BUSY-UNAVAILABLE",
        /// `BUSY-TENTATIVE`
        BusyTentative = "BUSY-TENTATIVE",
    }
}

#[cfg(test)]
mod tests {
    use chrono_tz::Europe::Berlin;
    use matrix_sdk::ruma::{events::StaticEventContent, owned_user_id};
    use serde_json::json;

    use super::*;
    use crate::events::datetime::parse_utc;

    #[test]
    fn policy_msc_example() {
        let v = json!({
          "type": "m.calendar.availability_policy",
          "state_key": "",
          "content": {
            "default_tier": 0,
            "grants": [
              { "subject": "@bob:example.com", "tier": 1 },
              { "subject": "*:trusted-corp.example", "tier": 1 }
            ]
          }
        });
        let content: AvailabilityPolicyEventContent =
            serde_json::from_value(v["content"].clone()).unwrap();
        let built = AvailabilityPolicyEventContent::new(
            TIER_NONE,
            vec![
                Grant::new("@bob:example.com", TIER_BUSY),
                Grant::new("*:trusted-corp.example", 1),
            ],
        );
        assert_eq!(content, built);
        assert_eq!(serde_json::to_value(&built).unwrap(), v["content"]);
        assert_eq!(
            AvailabilityPolicyEventContent::TYPE,
            "org.matrix.msc4496.availability_policy"
        );
    }

    #[test]
    fn window_msc_example() {
        let v = json!({
          "type": "m.calendar.availability_window",
          "state_key": "",
          "content": {
            "windows": [
              {
                "days": ["MO", "TU", "WE", "TH", "FR"],
                "start_time": "09:00",
                "end_time": "17:00",
                "timezone": "Europe/Berlin"
              }
            ]
          }
        });
        let content: AvailabilityWindowEventContent =
            serde_json::from_value(v["content"].clone()).unwrap();
        let built = AvailabilityWindowEventContent::new(vec![Window {
            days: vec![
                Weekday::Monday,
                Weekday::Tuesday,
                Weekday::Wednesday,
                Weekday::Thursday,
                Weekday::Friday,
            ],
            start_time: NaiveTime::from_hms_opt(9, 0, 0).unwrap(),
            end_time: NaiveTime::from_hms_opt(17, 0, 0).unwrap(),
            timezone: Berlin,
        }]);
        assert_eq!(content, built);
        assert_eq!(serde_json::to_value(&built).unwrap(), v["content"]);
        assert_eq!(
            AvailabilityWindowEventContent::TYPE,
            "org.matrix.msc4496.availability_window"
        );
    }

    #[test]
    fn api_msc_examples() {
        let req = json!({
          "range_start": "2026-07-01T00:00:00Z",
          "range_end": "2026-07-07T23:59:59Z"
        });
        let parsed: AvailabilityRequest = serde_json::from_value(req.clone()).unwrap();
        assert_eq!(serde_json::to_value(&parsed).unwrap(), req);

        let resp = json!({
          "user_id": "@alice:example.com",
          "range_start": "2026-07-01T00:00:00Z",
          "range_end": "2026-07-07T23:59:59Z",
          "busy": [
            {
              "start": "2026-07-01T07:00:00Z",
              "end": "2026-07-01T07:30:00Z",
              "type": "BUSY"
            },
            {
              "start": "2026-07-03T08:00:00Z",
              "end": "2026-07-03T17:00:00Z",
              "type": "BUSY-UNAVAILABLE"
            }
          ]
        });
        let parsed: AvailabilityResponse = serde_json::from_value(resp.clone()).unwrap();
        let built = AvailabilityResponse {
            user_id: owned_user_id!("@alice:example.com"),
            range_start: parse_utc("2026-07-01T00:00:00Z").unwrap(),
            range_end: parse_utc("2026-07-07T23:59:59Z").unwrap(),
            busy: vec![
                BusyInterval {
                    start: parse_utc("2026-07-01T07:00:00Z").unwrap(),
                    end: parse_utc("2026-07-01T07:30:00Z").unwrap(),
                    kind: BusyType::Busy,
                    title: None,
                },
                BusyInterval {
                    start: parse_utc("2026-07-03T08:00:00Z").unwrap(),
                    end: parse_utc("2026-07-03T17:00:00Z").unwrap(),
                    kind: BusyType::BusyUnavailable,
                    title: None,
                },
            ],
        };
        assert_eq!(parsed, built);
        assert_eq!(serde_json::to_value(&built).unwrap(), resp);
        assert_eq!(
            serde_json::to_value(BusyType::BusyTentative).unwrap(),
            json!("BUSY-TENTATIVE")
        );
    }
}
