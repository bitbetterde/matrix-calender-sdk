//! Event alerts, i.e. reminders (RFC 8984 §4.5.2).
//!
//! MSC4496 has no reminder concept, but declares alignment with JSCalendar (RFC 8984), so
//! alerts follow its `alerts` property: a map of alert ID to [`Alert`], each with a trigger
//! (when it fires) and an action (what the client does). This is an extension of the MSC, not
//! part of its text.
//!
//! Alerts live in the event content, so everyone in the calendar room sees the same ones, like
//! event-level reminders on a shared Google or Apple calendar. Scheduling the notifications is
//! up to the client; [`Alert::fire_time`] and
//! [`Occurrence::alert_times`](crate::recurrence::Occurrence::alert_times) compute when they
//! are due.

use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::{
    datetime::{EventTime, utc_serde},
    duration::SignedDuration,
};

/// The JSCalendar `@type` of an [`Alert`], written on output and optional on input.
pub const ALERT_TYPE: &str = "Alert";

string_enum! {
    /// What a client does when an [`Alert`] fires (RFC 8984 §4.5.2).
    ///
    /// Unknown values are kept as [`AlertAction::Other`] and should be ignored rather than
    /// treated like `display`, since the author asked for something else.
    #[derive(Default)]
    pub enum AlertAction {
        /// `display`: show a notification to the user (the default).
        #[default]
        Display = "display",
        /// `email`: send an email to the user.
        Email = "email",
    }
}

string_enum! {
    /// Whether an [`AlertTrigger::Offset`] is relative to the event's start or end.
    ///
    /// Unknown values are kept as [`AlertRelativeTo::Other`] and treated like `start`.
    #[derive(Default)]
    pub enum AlertRelativeTo {
        /// `start` (the default).
        #[default]
        Start = "start",
        /// `end`
        End = "end",
    }
}

/// A reminder on a calendar event.
///
/// Serializes to the JSCalendar shape, with `@type` always written and accepted but not
/// required on input:
///
/// ```json
/// { "@type": "Alert", "trigger": { "@type": "OffsetTrigger", "offset": "-PT15M" }, "action": "display" }
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Alert {
    /// When the alert fires.
    pub trigger: AlertTrigger,
    /// What the client does; `display` when the JSON omits it.
    pub action: AlertAction,
}

impl Alert {
    /// An alert with `trigger` and the default `display` action.
    pub fn new(trigger: AlertTrigger) -> Self {
        Self {
            trigger,
            action: AlertAction::default(),
        }
    }

    /// Sets the action.
    pub fn with_action(mut self, action: AlertAction) -> Self {
        self.action = action;
        self
    }

    /// When this alert fires for an occurrence running from `start` to `end`.
    ///
    /// An [`AlertTrigger::Absolute`] trigger fires at its `when`. An [`AlertTrigger::Offset`]
    /// trigger adds its offset to the start or end of the occurrence (an unknown `relativeTo`
    /// counts as `start`, see [`AlertRelativeTo`]): to the stored UTC instant of a timed value,
    /// or to midnight of the date in `fallback_tz` for an all-day value (RFC 8984 §4.5.2),
    /// where the exclusive all-day `end` is the midnight after the last day. Midnights that do
    /// not exist because of a DST gap resolve as in [`EventTime::to_utc`].
    ///
    /// Saturates instead of overflowing at the edges of the supported range.
    pub fn fire_time(&self, start: &EventTime, end: &EventTime, fallback_tz: Tz) -> DateTime<Utc> {
        match &self.trigger {
            AlertTrigger::Absolute { when } => *when,
            AlertTrigger::Offset {
                offset,
                relative_to,
            } => {
                let base = match relative_to {
                    AlertRelativeTo::End => end,
                    _ => start,
                }
                .to_utc(fallback_tz);
                base.checked_add_signed(offset.as_time_delta())
                    .unwrap_or(base)
            }
        }
    }
}

/// When an [`Alert`] fires, tagged by its JSCalendar `@type`.
///
/// ```json
/// { "@type": "OffsetTrigger", "offset": "-PT15M", "relativeTo": "start" }
/// { "@type": "AbsoluteTrigger", "when": "2026-10-04T08:45:00Z" }
/// ```
///
/// `relativeTo` defaults to `start`; an unknown or missing `@type` is a deserialization error.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "@type")]
pub enum AlertTrigger {
    /// An `OffsetTrigger`: a duration before (negative) or after the event's start or end.
    #[serde(rename = "OffsetTrigger")]
    Offset {
        /// The offset, negative for an alert before the reference point.
        offset: SignedDuration,
        /// Whether `offset` applies to the event's start or end; `start` when the JSON omits
        /// it.
        #[serde(rename = "relativeTo", default)]
        relative_to: AlertRelativeTo,
    },
    /// An `AbsoluteTrigger`: a fixed UTC instant, independent of the event's times.
    #[serde(rename = "AbsoluteTrigger")]
    Absolute {
        /// The instant the alert fires at.
        #[serde(with = "utc_serde")]
        when: DateTime<Utc>,
    },
}

impl AlertTrigger {
    /// An offset trigger relative to the event's start, e.g. `-PT15M` for 15 minutes before it.
    pub fn offset(offset: SignedDuration) -> Self {
        Self::Offset {
            offset,
            relative_to: AlertRelativeTo::Start,
        }
    }

    /// An offset trigger relative to the event's end.
    pub fn offset_from_end(offset: SignedDuration) -> Self {
        Self::Offset {
            offset,
            relative_to: AlertRelativeTo::End,
        }
    }

    /// An absolute trigger firing at `when`.
    pub fn absolute(when: DateTime<Utc>) -> Self {
        Self::Absolute { when }
    }
}

#[derive(Serialize)]
struct AlertSer<'a> {
    #[serde(rename = "@type")]
    kind: &'static str,
    trigger: &'a AlertTrigger,
    action: &'a AlertAction,
}

impl Serialize for Alert {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        AlertSer {
            kind: ALERT_TYPE,
            trigger: &self.trigger,
            action: &self.action,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Alert {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Helper {
            #[serde(rename = "@type")]
            kind: Option<String>,
            trigger: AlertTrigger,
            #[serde(default)]
            action: AlertAction,
        }

        let Helper {
            kind,
            trigger,
            action,
        } = Helper::deserialize(deserializer)?;
        if let Some(kind) = kind
            && kind != ALERT_TYPE
        {
            return Err(serde::de::Error::custom(format!(
                "expected @type {ALERT_TYPE}, got {kind}"
            )));
        }
        Ok(Self { trigger, action })
    }
}

#[cfg(test)]
mod tests {
    use chrono_tz::Europe::Berlin;
    use serde_json::json;

    use super::*;
    use crate::events::datetime::parse_utc;

    fn duration(s: &str) -> SignedDuration {
        s.parse().expect("valid duration")
    }

    fn timed(local: &str) -> EventTime {
        EventTime::timed(local.parse().expect("valid localtime"), Berlin).expect("exists")
    }

    fn all_day(date: &str) -> EventTime {
        EventTime::all_day(date.parse().expect("valid date"))
    }

    #[test]
    fn offset_alert_round_trips() {
        let v = json!({
            "@type": "Alert",
            "trigger": { "@type": "OffsetTrigger", "offset": "-PT15M", "relativeTo": "start" },
            "action": "display"
        });
        let alert: Alert = serde_json::from_value(v.clone()).expect("valid alert");
        assert_eq!(alert, Alert::new(AlertTrigger::offset(duration("-PT15M"))));
        assert_eq!(serde_json::to_value(&alert).expect("serializable"), v);
    }

    #[test]
    fn absolute_alert_round_trips() {
        let v = json!({
            "@type": "Alert",
            "trigger": { "@type": "AbsoluteTrigger", "when": "2026-10-04T08:45:00Z" },
            "action": "email"
        });
        let alert: Alert = serde_json::from_value(v.clone()).expect("valid alert");
        assert_eq!(
            alert,
            Alert::new(AlertTrigger::absolute(
                parse_utc("2026-10-04T08:45:00Z").expect("valid instant")
            ))
            .with_action(AlertAction::Email)
        );
        assert_eq!(serde_json::to_value(&alert).expect("serializable"), v);
    }

    #[test]
    fn type_is_optional_and_defaults_are_filled_in() {
        let alert: Alert = serde_json::from_value(json!({
            "trigger": { "@type": "OffsetTrigger", "offset": "PT0S" }
        }))
        .expect("valid alert");
        assert_eq!(alert.action, AlertAction::Display);
        assert_eq!(
            alert.trigger,
            AlertTrigger::Offset {
                offset: SignedDuration::ZERO,
                relative_to: AlertRelativeTo::Start,
            }
        );
        // The defaults and `@type` are written out explicitly.
        assert_eq!(
            serde_json::to_value(&alert).expect("serializable"),
            json!({
                "@type": "Alert",
                "trigger": { "@type": "OffsetTrigger", "offset": "PT0S", "relativeTo": "start" },
                "action": "display"
            })
        );
    }

    #[test]
    fn unknown_action_round_trips() {
        let v = json!({
            "@type": "Alert",
            "trigger": { "@type": "OffsetTrigger", "offset": "-PT15M", "relativeTo": "end" },
            "action": "x-sms"
        });
        let alert: Alert = serde_json::from_value(v.clone()).expect("valid alert");
        assert_eq!(alert.action, AlertAction::Other("x-sms".into()));
        assert_eq!(serde_json::to_value(&alert).expect("serializable"), v);
    }

    #[test]
    fn wrong_or_unknown_types_are_rejected() {
        // A wrong `@type` on the alert.
        assert!(
            serde_json::from_value::<Alert>(json!({
                "@type": "Event",
                "trigger": { "@type": "OffsetTrigger", "offset": "-PT15M" }
            }))
            .is_err()
        );
        // An unknown trigger type is not silently treated as an offset trigger.
        assert!(
            serde_json::from_value::<Alert>(json!({
                "trigger": { "@type": "UnknownTrigger", "offset": "-PT15M" }
            }))
            .is_err()
        );
        // A missing trigger type, a missing trigger and a malformed offset.
        assert!(
            serde_json::from_value::<Alert>(json!({ "trigger": { "offset": "-PT15M" } })).is_err()
        );
        assert!(serde_json::from_value::<Alert>(json!({ "action": "display" })).is_err());
        assert!(
            serde_json::from_value::<Alert>(json!({
                "trigger": { "@type": "OffsetTrigger", "offset": "15 minutes" }
            }))
            .is_err()
        );
    }

    #[test]
    fn fire_time_of_a_timed_event() {
        let start = timed("2026-07-01T09:00:00");
        let end = timed("2026-07-01T09:30:00");
        let utc = |s: &str| parse_utc(s).expect("valid instant");

        let before_start = Alert::new(AlertTrigger::offset(duration("-PT15M")));
        assert_eq!(
            before_start.fire_time(&start, &end, Berlin),
            utc("2026-07-01T06:45:00Z")
        );
        // The fallback timezone does not matter for timed events.
        assert_eq!(
            before_start.fire_time(&start, &end, Tz::UTC),
            utc("2026-07-01T06:45:00Z")
        );

        let after_end = Alert::new(AlertTrigger::offset_from_end(duration("PT1H")));
        assert_eq!(
            after_end.fire_time(&start, &end, Berlin),
            utc("2026-07-01T08:30:00Z")
        );

        let at_start = Alert::new(AlertTrigger::offset(SignedDuration::ZERO));
        assert_eq!(
            at_start.fire_time(&start, &end, Berlin),
            utc("2026-07-01T07:00:00Z")
        );

        let absolute = Alert::new(AlertTrigger::absolute(utc("2026-06-30T20:00:00Z")));
        assert_eq!(
            absolute.fire_time(&start, &end, Berlin),
            utc("2026-06-30T20:00:00Z")
        );
    }

    #[test]
    fn fire_time_of_an_all_day_event_uses_the_fallback_timezone() {
        // 2026-10-05 and 2026-10-06, the `end` being exclusive.
        let start = all_day("2026-10-05");
        let end = all_day("2026-10-07");
        let utc = |s: &str| parse_utc(s).expect("valid instant");

        // Midnight Berlin (CEST, +02:00) on the start date is 2026-10-04T22:00Z, 15 hours
        // earlier is 09:00 Berlin on the day before.
        let evening_before = Alert::new(AlertTrigger::offset(duration("-PT15H")));
        assert_eq!(
            evening_before.fire_time(&start, &end, Berlin),
            utc("2026-10-04T07:00:00Z")
        );
        assert_eq!(
            evening_before.fire_time(&start, &end, Tz::UTC),
            utc("2026-10-04T09:00:00Z")
        );

        // Relative to the end: the midnight after the last day (2026-10-06), minus an hour.
        let after_last_day = Alert::new(AlertTrigger::offset_from_end(duration("-PT1H")));
        assert_eq!(
            after_last_day.fire_time(&start, &end, Berlin),
            utc("2026-10-06T21:00:00Z")
        );

        // Across the Berlin DST change (CEST -> CET on 2026-10-25): midnight on 2026-10-26 is
        // 2026-10-25T23:00Z, and a day earlier is 2026-10-24T23:00Z, i.e. 01:00 local on the
        // 25th, because that day has 25 hours. The offset counts elapsed time, not local time.
        let after_dst = all_day("2026-10-26");
        let day_before = Alert::new(AlertTrigger::offset(duration("-P1D")));
        assert_eq!(
            day_before.fire_time(&after_dst, &after_dst, Berlin),
            utc("2026-10-24T23:00:00Z")
        );

        // Cuba starts DST at midnight, so 2026-03-08 00:00 does not exist in Havana; the
        // midnight of an all-day event there resolves leniently, as `EventTime::to_utc` does.
        let havana = "America/Havana".parse::<Tz>().expect("known timezone");
        let gap = all_day("2026-03-08");
        assert_eq!(
            Alert::new(AlertTrigger::offset(SignedDuration::ZERO)).fire_time(&gap, &gap, havana),
            utc("2026-03-08T05:00:00Z")
        );
    }

    #[test]
    fn unknown_relative_to_counts_as_start() {
        let alert = Alert::new(AlertTrigger::Offset {
            offset: duration("-PT15M"),
            relative_to: AlertRelativeTo::Other("middle".into()),
        });
        let start = timed("2026-07-01T09:00:00");
        let end = timed("2026-07-01T17:00:00");
        assert_eq!(
            alert.fire_time(&start, &end, Berlin),
            parse_utc("2026-07-01T06:45:00Z").expect("valid instant")
        );
    }
}
