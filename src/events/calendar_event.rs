//! The `calendar_event` message-like event (MSC4496 §2).

use chrono::{DateTime, Utc};
use matrix_sdk::ruma::{
    OwnedEventId, OwnedUserId,
    events::{macros::EventContent, message::TextContentBlock, relation::Replacement},
    serde::JsonObject,
};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de, ser::SerializeMap};

use super::{
    conference::Conference,
    datetime::{EventTime, utc_serde},
    location::Location,
    recurrence::Recurrence,
    text::{calendar_event_text, serialize_text_block},
};
use crate::{
    Error, Result,
    ids::{CALENDAR_OVERRIDE_REL_TYPE_UNSTABLE, is_calendar_override_rel_type},
};

/// Content of the `org.matrix.msc4496.calendar_event` (stable: `m.calendar.event`) timeline
/// event.
///
/// Build it with [`CalendarEventEventContent::new`], turn it into an edit with
/// [`CalendarEventEventContent::into_replacement`], or build an instance override with
/// [`CalendarEventEventContent::override_of`].
#[derive(Clone, Debug, Serialize, Deserialize, EventContent)]
#[ruma_event(
    type = "org.matrix.msc4496.calendar_event",
    alias = "m.calendar.event",
    kind = MessageLike,
    without_relation
)]
pub struct CalendarEventEventContent {
    /// Human-readable fallback.
    ///
    /// Defaults to empty on input (it may be omitted in E2EE rooms) and is omitted on output
    /// when empty.
    #[serde(
        rename = "m.text",
        alias = "org.matrix.msc1767.text",
        default,
        skip_serializing_if = "TextContentBlock::is_empty",
        serialize_with = "serialize_text_block"
    )]
    pub text: TextContentBlock,

    /// The structured calendar event.
    #[serde(
        rename = "org.matrix.msc4496.calendar_event",
        alias = "m.calendar.event"
    )]
    pub event: CalendarEvent,

    /// An `m.replace` edit or an instance-override relation, if any.
    #[serde(
        flatten,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_relation"
    )]
    pub relates_to: Option<CalendarEventRelation>,
}

impl CalendarEventEventContent {
    /// Creates the content for `event`, generating the `m.text` fallback.
    pub fn new(event: CalendarEvent) -> Self {
        Self {
            text: calendar_event_text(&event),
            event,
            relates_to: None,
        }
    }

    /// Creates an instance override of the recurring series whose original event is
    /// `series_event_id`.
    ///
    /// `event` should carry the series' `uid` and a `recurrence_id` identifying the overridden
    /// instance; use [`CalendarEvent::validate_override`] to check.
    pub fn override_of(series_event_id: OwnedEventId, event: CalendarEvent) -> Self {
        Self {
            relates_to: Some(CalendarEventRelation::Override(CalendarOverride::new(
                series_event_id,
            ))),
            ..Self::new(event)
        }
    }

    /// Turns this content into an `m.replace` edit of `original_event_id`.
    ///
    /// The result carries the complete new content in `m.new_content`; the top-level
    /// `m.text` is prefixed with `* ` as the Matrix spec suggests for edit fallbacks. Any
    /// relation of `self` is not carried over (relations cannot be changed by edits).
    pub fn into_replacement(self, original_event_id: OwnedEventId) -> Self {
        let new_content = CalendarEventEventContentWithoutRelation::from(self.clone());
        let text = self
            .text
            .iter()
            .map(|r| {
                let mut r = r.clone();
                r.body = format!("* {}", r.body);
                r
            })
            .collect();
        Self {
            text,
            event: self.event,
            relates_to: Some(CalendarEventRelation::Replacement(Replacement::new(
                original_event_id,
                new_content,
            ))),
        }
    }

    /// The new content if this is an `m.replace` edit.
    pub fn new_content(&self) -> Option<&CalendarEventEventContentWithoutRelation> {
        match &self.relates_to {
            Some(CalendarEventRelation::Replacement(r)) => Some(&r.new_content),
            _ => None,
        }
    }
}

impl CalendarEventEventContentWithoutRelation {
    /// Creates the content for `event` without relation, generating the `m.text` fallback.
    pub fn new(event: CalendarEvent) -> Self {
        CalendarEventEventContent::new(event).into()
    }
}

/// Relations a calendar event can carry.
#[derive(Clone, Debug)]
// Edits are the common case; boxing the new content would only complicate matching.
#[allow(clippy::large_enum_variant)]
pub enum CalendarEventRelation {
    /// An `m.replace` edit; the new content is serialized as `m.new_content`.
    Replacement(Replacement<CalendarEventEventContentWithoutRelation>),
    /// An instance override (`org.matrix.msc4496.calendar_override` /
    /// `m.calendar.override`) of a recurring series.
    Override(CalendarOverride),
    /// Any other relation, kept verbatim as the `m.relates_to` JSON object.
    Other(JsonObject),
}

impl CalendarEventRelation {
    /// The event ID this relation points at, if any.
    pub fn event_id(&self) -> Option<&OwnedEventId> {
        match self {
            Self::Replacement(r) => Some(&r.event_id),
            Self::Override(o) => Some(&o.event_id),
            Self::Other(_) => None,
        }
    }
}

/// An instance-override relation pointing at the original (series) event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CalendarOverride {
    /// The event ID of the recurring series' original event.
    pub event_id: OwnedEventId,
}

impl CalendarOverride {
    /// Creates an override relation to `event_id`.
    pub fn new(event_id: OwnedEventId) -> Self {
        Self { event_id }
    }
}

#[derive(Serialize)]
struct RelatesToSer<'a> {
    rel_type: &'a str,
    event_id: &'a OwnedEventId,
}

impl Serialize for CalendarEventRelation {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(None)?;
        match self {
            Self::Replacement(r) => {
                map.serialize_entry(
                    "m.relates_to",
                    &RelatesToSer {
                        rel_type: "m.replace",
                        event_id: &r.event_id,
                    },
                )?;
                map.serialize_entry("m.new_content", &r.new_content)?;
            }
            Self::Override(o) => {
                map.serialize_entry(
                    "m.relates_to",
                    &RelatesToSer {
                        rel_type: CALENDAR_OVERRIDE_REL_TYPE_UNSTABLE,
                        event_id: &o.event_id,
                    },
                )?;
            }
            Self::Other(obj) => map.serialize_entry("m.relates_to", obj)?,
        }
        map.end()
    }
}

fn deserialize_relation<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<CalendarEventRelation>, D::Error> {
    #[derive(Deserialize)]
    struct Helper {
        #[serde(rename = "m.relates_to")]
        relates_to: Option<JsonObject>,
        #[serde(rename = "m.new_content")]
        new_content: Option<serde_json::Value>,
    }

    #[derive(Deserialize)]
    struct EventIdHelper {
        event_id: OwnedEventId,
    }

    let Helper {
        relates_to,
        new_content,
    } = Helper::deserialize(deserializer)?;
    let Some(relates_to) = relates_to else {
        return Ok(None);
    };
    let rel_type = relates_to
        .get("rel_type")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let event_id = || {
        serde_json::from_value::<EventIdHelper>(relates_to.clone().into())
            .map(|h| h.event_id)
            .map_err(de::Error::custom)
    };

    let relation = if rel_type == "m.replace" {
        let new_content = new_content.ok_or_else(|| de::Error::missing_field("m.new_content"))?;
        let new_content = serde_json::from_value(new_content).map_err(de::Error::custom)?;
        CalendarEventRelation::Replacement(Replacement::new(event_id()?, new_content))
    } else if is_calendar_override_rel_type(rel_type) {
        CalendarEventRelation::Override(CalendarOverride::new(event_id()?))
    } else {
        CalendarEventRelation::Other(relates_to)
    };
    Ok(Some(relation))
}

string_enum! {
    /// Event status (iCalendar `STATUS`).
    ///
    /// Unknown values are kept as [`Status::Other`] and treated like `confirmed`.
    #[derive(Default)]
    pub enum Status {
        /// `confirmed` (the default).
        #[default]
        Confirmed = "confirmed",
        /// `tentative`
        Tentative = "tentative",
        /// `cancelled`
        Cancelled = "cancelled",
    }
}

string_enum! {
    /// Whether the event blocks time in availability (iCalendar `TRANSP`).
    ///
    /// Unknown values are kept as [`Transparency::Other`] and treated like `opaque`.
    #[derive(Default)]
    pub enum Transparency {
        /// `opaque`: blocks availability (the default).
        #[default]
        Opaque = "opaque",
        /// `transparent`: does not block availability.
        Transparent = "transparent",
    }
}

/// The `calendar_event` content block.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CalendarEvent {
    /// Stable identifier across edits, recurrences and CalDAV round-trips. Unique only per
    /// sender.
    pub uid: String,
    /// Event title.
    pub title: String,
    /// Start time.
    pub start: EventTime,
    /// End time (exclusive for all-day events).
    pub end: EventTime,
    /// Whether this is an all-day event; then `start`/`end` are dates.
    #[serde(default)]
    pub all_day: bool,
    /// Status; defaults to `confirmed`.
    #[serde(default)]
    pub status: Status,
    /// Transparency; defaults to `opaque`.
    #[serde(default)]
    pub transparency: Transparency,
    /// Optional description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The organiser's Matrix ID.
    ///
    /// Optional on input (the MSC's §5.1 example omits it) and omitted on output when absent.
    /// A missing organiser means the event's sender; see
    /// [`IndexedEvent::organizer`](crate::calendar::IndexedEvent::organizer).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub organizer: Option<OwnedUserId>,
    /// Optional location. Serialized as `null` when absent, as in the MSC example.
    #[serde(default)]
    pub location: Option<Location>,
    /// Optional MatrixRTC conference. Serialized as `null` when absent.
    #[serde(default)]
    pub conference: Option<Conference>,
    /// Optional recurrence. Serialized as `null` when absent.
    #[serde(default)]
    pub recurrence: Option<Recurrence>,
    /// For instance overrides: the original UTC start of the overridden occurrence.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "utc_serde::option"
    )]
    pub recurrence_id: Option<DateTime<Utc>>,
}

impl CalendarEvent {
    /// Creates an event with a fresh UUID v4 `uid`, default status/transparency and no
    /// optional fields. `all_day` is derived from `start`.
    ///
    /// `organizer` accepts an [`OwnedUserId`] or an `Option<OwnedUserId>` (`None` leaves the
    /// organiser implicit, i.e. the sender).
    pub fn new(
        title: impl Into<String>,
        start: EventTime,
        end: EventTime,
        organizer: impl Into<Option<OwnedUserId>>,
    ) -> Self {
        Self {
            uid: uuid::Uuid::new_v4().to_string(),
            title: title.into(),
            all_day: start.is_all_day(),
            start,
            end,
            status: Status::default(),
            transparency: Transparency::default(),
            description: None,
            organizer: organizer.into(),
            location: None,
            conference: None,
            recurrence: None,
            recurrence_id: None,
        }
    }

    /// Whether this event recurs.
    pub fn is_recurring(&self) -> bool {
        self.recurrence.is_some()
    }

    /// Whether this event is an instance override (has a `recurrence_id`).
    pub fn is_override(&self) -> bool {
        self.recurrence_id.is_some()
    }

    /// Whether this event is cancelled.
    pub fn is_cancelled(&self) -> bool {
        self.status == Status::Cancelled
    }

    /// Checks internal consistency:
    ///
    /// - `uid` and `title` are not empty,
    /// - `start` and `end` are both timed or both all-day, matching `all_day`,
    /// - timed values have consistent `utc`/`localtime`,
    /// - `end` is after `start` (all-day: at least one day later, as `end` is exclusive),
    /// - the recurrence rule, if any, is valid.
    pub fn validate(&self) -> Result<()> {
        if self.uid.is_empty() {
            return Err(Error::InvalidIdentifier("empty uid".into()));
        }
        if self.title.is_empty() {
            return Err(Error::Other("empty title".into()));
        }
        self.start.validate()?;
        self.end.validate()?;
        match (&self.start, &self.end) {
            (EventTime::AllDay { date: s }, EventTime::AllDay { date: e }) => {
                if !self.all_day {
                    return Err(Error::InconsistentDateTime(
                        "start/end are dates but all_day is false".into(),
                    ));
                }
                if e <= s {
                    return Err(Error::InconsistentDateTime(
                        "all-day end (exclusive) must be after start".into(),
                    ));
                }
            }
            (EventTime::Timed { utc: s, .. }, EventTime::Timed { utc: e, .. }) => {
                if self.all_day {
                    return Err(Error::InconsistentDateTime(
                        "all_day is true but start/end are timed".into(),
                    ));
                }
                if e < s {
                    return Err(Error::InconsistentDateTime("end is before start".into()));
                }
            }
            _ => {
                return Err(Error::InconsistentDateTime(
                    "start and end must both be timed or both be all-day".into(),
                ));
            }
        }
        if let Some(r) = &self.recurrence {
            r.rrule.validate()?;
        }
        Ok(())
    }

    /// Like [`CalendarEvent::validate`], and additionally checks that `recurrence_id` is set
    /// and `recurrence` is not (an override describes a single instance).
    pub fn validate_override(&self) -> Result<()> {
        self.validate()?;
        if self.recurrence_id.is_none() {
            return Err(Error::Other("override is missing recurrence_id".into()));
        }
        if self.recurrence.is_some() {
            return Err(Error::Other(
                "override must not carry a recurrence rule".into(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use chrono_tz::Europe::Berlin;
    use matrix_sdk::ruma::{
        events::{MessageLikeEventType, StaticEventContent},
        owned_event_id, owned_user_id,
    };
    use serde_json::{Value, json};

    use super::*;
    use crate::events::{
        datetime::parse_utc,
        recurrence::{ByDay, Frequency, RRule, Until, Weekday},
    };

    fn msc_event_json() -> Value {
        json!({
          "type": "m.calendar.event",
          "content": {
            "m.text": [
              {
                "body": "Team standup – 2026-07-01 09:00–09:30 (Europe/Berlin)",
                "mimetype": "text/plain"
              }
            ],
            "m.calendar.event": {
              "uid": "550e8400-e29b-41d4-a716-446655440000",
              "title": "Team standup",
              "start": {
                "utc": "2026-07-01T07:00:00Z",
                "localtime": "2026-07-01T09:00:00",
                "timezone": "Europe/Berlin"
              },
              "end": {
                "utc": "2026-07-01T07:30:00Z",
                "localtime": "2026-07-01T09:30:00",
                "timezone": "Europe/Berlin"
              },
              "all_day": false,
              "status": "confirmed",
              "transparency": "opaque",
              "description": "Daily sync with engineering team.",
              "organizer": "@alice:example.com",
              "location": null,
              "conference": null,
              "recurrence": null
            }
          }
        })
    }

    fn standup() -> CalendarEvent {
        let mut ev = CalendarEvent::new(
            "Team standup",
            EventTime::timed("2026-07-01T09:00:00".parse().unwrap(), Berlin).unwrap(),
            EventTime::timed("2026-07-01T09:30:00".parse().unwrap(), Berlin).unwrap(),
            owned_user_id!("@alice:example.com"),
        );
        ev.uid = "550e8400-e29b-41d4-a716-446655440000".into();
        ev.description = Some("Daily sync with engineering team.".into());
        ev
    }

    /// The MSC example with unstable names, as the SDK sends it.
    fn unstable(mut v: Value) -> Value {
        let content = v["content"].as_object_mut().unwrap();
        let block = content.remove("m.calendar.event").unwrap();
        content.insert("org.matrix.msc4496.calendar_event".into(), block);
        v["content"].take()
    }

    #[test]
    fn msc_example_deserializes() {
        let v = msc_event_json();
        let content: CalendarEventEventContent =
            serde_json::from_value(v["content"].clone()).unwrap();
        assert_eq!(content.event, standup());
        assert!(content.relates_to.is_none());
        content.event.validate().unwrap();
        assert_eq!(
            content.text.find_plain(),
            Some("Team standup – 2026-07-01 09:00–09:30 (Europe/Berlin)")
        );
    }

    #[test]
    fn builder_matches_msc_example() {
        let content = CalendarEventEventContent::new(standup());
        assert_eq!(
            serde_json::to_value(&content).unwrap(),
            unstable(msc_event_json())
        );
        assert_eq!(
            CalendarEventEventContent::TYPE,
            "org.matrix.msc4496.calendar_event"
        );
        assert_eq!(
            MessageLikeEventType::from(CalendarEventEventContent::TYPE),
            MessageLikeEventType::from("org.matrix.msc4496.calendar_event")
        );
    }

    /// The decrypted payload from MSC §5.1, verbatim (no `organizer`, no optional fields).
    fn msc_encrypted_payload_json() -> Value {
        json!({
          "type": "m.calendar.event",
          "content": {
            "m.text": [
              {
                "body": "Team standup – 2026-07-01 09:00–09:30 (Europe/Berlin)",
                "mimetype": "text/plain"
              }
            ],
            "m.calendar.event": {
              "uid": "550e8400-e29b-41d4-a716-446655440000",
              "title": "Team standup",
              "start": { "utc": "2026-07-01T07:00:00Z", "localtime": "2026-07-01T09:00:00", "timezone": "Europe/Berlin" },
              "end": { "utc": "2026-07-01T07:30:00Z", "localtime": "2026-07-01T09:30:00", "timezone": "Europe/Berlin" }
            }
          }
        })
    }

    #[test]
    fn encrypted_payload_example_with_missing_optionals() {
        let content: CalendarEventEventContent =
            serde_json::from_value(msc_encrypted_payload_json()["content"].clone()).unwrap();
        assert!(!content.event.all_day);
        assert_eq!(content.event.status, Status::Confirmed);
        assert_eq!(content.event.transparency, Transparency::Opaque);
        assert_eq!(content.event.location, None);
        assert_eq!(content.event.organizer, None);
        assert_eq!(content.event.description, None);
        content.event.validate().unwrap();
    }

    #[test]
    fn encrypted_payload_example_round_trips() {
        let v = msc_encrypted_payload_json();
        let content: CalendarEventEventContent =
            serde_json::from_value(v["content"].clone()).unwrap();
        let out = serde_json::to_value(&content).unwrap();
        // Defaults are written out explicitly (and the `null`s the MSC §2.1 example uses);
        // everything else is unchanged and nothing is invented, in particular no organiser.
        let mut expected = unstable(v);
        let block = expected["org.matrix.msc4496.calendar_event"]
            .as_object_mut()
            .unwrap();
        for (k, val) in [
            ("all_day", json!(false)),
            ("status", json!("confirmed")),
            ("transparency", json!("opaque")),
            ("location", Value::Null),
            ("conference", Value::Null),
            ("recurrence", Value::Null),
        ] {
            block.insert(k.into(), val);
        }
        assert_eq!(out, expected);
        assert!(
            out["org.matrix.msc4496.calendar_event"]
                .get("organizer")
                .is_none()
        );
        // And back again.
        let back: CalendarEventEventContent = serde_json::from_value(out).unwrap();
        assert_eq!(back.event, content.event);
    }

    #[test]
    fn unknown_enum_values_round_trip() {
        let mut v = unstable(msc_event_json());
        v["org.matrix.msc4496.calendar_event"]["status"] = json!("x-postponed");
        v["org.matrix.msc4496.calendar_event"]["transparency"] = json!("x-partial");
        let parsed: CalendarEventEventContent = serde_json::from_value(v.clone()).unwrap();
        assert_eq!(parsed.event.status, Status::Other("x-postponed".into()));
        assert_eq!(
            parsed.event.transparency,
            Transparency::Other("x-partial".into())
        );
        assert!(!parsed.event.is_cancelled());
        assert_eq!(serde_json::to_value(&parsed).unwrap(), v);
    }

    #[test]
    fn missing_text_is_tolerated_and_omitted() {
        let content: CalendarEventEventContent = serde_json::from_value(json!({
            "org.matrix.msc4496.calendar_event": unstable(msc_event_json())["org.matrix.msc4496.calendar_event"]
        }))
        .unwrap();
        assert!(content.text.is_empty());
        let out = serde_json::to_value(&content).unwrap();
        assert!(out.get("m.text").is_none());
    }

    #[test]
    fn all_day_example() {
        let ev: CalendarEvent = serde_json::from_value(json!({
            "uid": "u1",
            "title": "Christmas",
            "start": { "date": "2026-12-25" },
            "end":   { "date": "2026-12-26" },
            "all_day": true,
            "organizer": "@alice:example.com"
        }))
        .unwrap();
        ev.validate().unwrap();
        let built = CalendarEvent::new(
            "Christmas",
            EventTime::all_day("2026-12-25".parse().unwrap()),
            EventTime::all_day("2026-12-26".parse().unwrap()),
            owned_user_id!("@alice:example.com"),
        );
        assert!(built.all_day);
        assert_eq!(
            CalendarEventEventContent::new(built).text.find_plain(),
            Some("Christmas – 2026-12-25 (all day)")
        );
    }

    #[test]
    fn recurrence_and_override_example() {
        let mut series = standup();
        series.recurrence = Some(Recurrence {
            rrule: RRule {
                byday: Some(vec![ByDay::every(Weekday::Monday)]),
                until: Some(Until::Date("2026-12-31".parse().unwrap())),
                ..RRule::new(Frequency::Weekly)
            },
            exdates: vec![parse_utc("2026-08-05T07:00:00Z").unwrap()],
            rdates: vec![],
        });
        series.validate().unwrap();
        let v = serde_json::to_value(CalendarEventEventContent::new(series.clone())).unwrap();
        assert_eq!(
            v["org.matrix.msc4496.calendar_event"]["recurrence"],
            json!({
              "rrule": { "freq": "WEEKLY", "byday": ["MO"], "until": "2026-12-31" },
              "exdates": ["2026-08-05T07:00:00Z"]
            })
        );

        // Override, as in the MSC (stable rel_type accepted on input).
        let mut msc = unstable(msc_event_json());
        msc["m.relates_to"] =
            json!({ "rel_type": "m.calendar.override", "event_id": "$original_event_id" });
        msc["org.matrix.msc4496.calendar_event"]["recurrence_id"] = json!("2026-07-08T07:00:00Z");
        let parsed: CalendarEventEventContent = serde_json::from_value(msc.clone()).unwrap();
        let Some(CalendarEventRelation::Override(o)) = &parsed.relates_to else {
            panic!("expected override relation, got {:?}", parsed.relates_to);
        };
        assert_eq!(o.event_id, "$original_event_id");
        assert_eq!(
            parsed.event.recurrence_id,
            Some(parse_utc("2026-07-08T07:00:00Z").unwrap())
        );

        // The builder emits the unstable rel_type.
        let mut instance = standup();
        instance.recurrence_id = Some(parse_utc("2026-07-08T07:00:00Z").unwrap());
        instance.validate_override().unwrap();
        let built =
            CalendarEventEventContent::override_of(owned_event_id!("$original_event_id"), instance);
        msc["m.relates_to"]["rel_type"] = json!("org.matrix.msc4496.calendar_override");
        assert_eq!(serde_json::to_value(&built).unwrap(), msc);
        assert!(series.validate_override().is_err());
    }

    #[test]
    fn replacement_round_trip() {
        let mut ev = standup();
        ev.status = Status::Cancelled;
        let edit = CalendarEventEventContent::new(ev.clone())
            .into_replacement(owned_event_id!("$original"));
        let v = serde_json::to_value(&edit).unwrap();
        let text = "Cancelled: Team standup – 2026-07-01 09:00–09:30 (Europe/Berlin)";
        let block = serde_json::to_value(&ev).unwrap();
        assert_eq!(
            v,
            json!({
                "m.text": [{ "body": format!("* {text}"), "mimetype": "text/plain" }],
                "org.matrix.msc4496.calendar_event": block,
                "m.new_content": {
                    "m.text": [{ "body": text, "mimetype": "text/plain" }],
                    "org.matrix.msc4496.calendar_event": block,
                },
                "m.relates_to": { "rel_type": "m.replace", "event_id": "$original" }
            })
        );

        let back: CalendarEventEventContent = serde_json::from_value(v).unwrap();
        let Some(CalendarEventRelation::Replacement(r)) = &back.relates_to else {
            panic!("expected replacement");
        };
        assert_eq!(r.event_id, "$original");
        assert_eq!(back.new_content().unwrap().event, ev);
        assert!(back.new_content().unwrap().event.is_cancelled());

        // m.replace without m.new_content is rejected, like ruma does.
        let mut bad = serde_json::to_value(&edit).unwrap();
        bad.as_object_mut().unwrap().remove("m.new_content");
        assert!(serde_json::from_value::<CalendarEventEventContent>(bad).is_err());
    }

    #[test]
    fn unknown_relation_is_preserved() {
        let mut v = unstable(msc_event_json());
        v["m.relates_to"] = json!({ "rel_type": "m.thread", "event_id": "$root" });
        let parsed: CalendarEventEventContent = serde_json::from_value(v.clone()).unwrap();
        assert!(matches!(
            parsed.relates_to,
            Some(CalendarEventRelation::Other(_))
        ));
        assert_eq!(serde_json::to_value(&parsed).unwrap(), v);
    }

    #[test]
    fn validate_catches_inconsistencies() {
        let mut ev = standup();
        std::mem::swap(&mut ev.start, &mut ev.end);
        assert!(ev.validate().is_err());

        let mut ev = standup();
        ev.all_day = true;
        assert!(ev.validate().is_err());

        let mut ev = standup();
        ev.end = EventTime::all_day("2026-07-02".parse().unwrap());
        assert!(ev.validate().is_err());
    }
}
