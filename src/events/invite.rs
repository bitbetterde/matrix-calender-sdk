//! The `calendar_invite` message-like event (MSC4496 §3.1).

use matrix_sdk::ruma::{
    OwnedUserId,
    events::{macros::EventContent, message::TextContentBlock},
};
use serde::{Deserialize, Serialize};

use super::{
    calendar_event::CalendarEvent,
    conference::Conference,
    datetime::EventTime,
    location::Location,
    recurrence::Recurrence,
    text::{invite_text, serialize_text_block},
};

/// Content of the `org.matrix.msc4496.calendar_invite` (stable: `m.calendar.invite`) timeline
/// event, sent by the organiser into a room shared with the invitee(s).
#[derive(Clone, Debug, Serialize, Deserialize, EventContent)]
#[ruma_event(
    type = "org.matrix.msc4496.calendar_invite",
    alias = "m.calendar.invite",
    kind = MessageLike
)]
pub struct CalendarInviteEventContent {
    /// Human-readable fallback.
    #[serde(
        rename = "m.text",
        alias = "org.matrix.msc1767.text",
        default,
        skip_serializing_if = "TextContentBlock::is_empty",
        serialize_with = "serialize_text_block"
    )]
    pub text: TextContentBlock,

    /// The structured invite.
    #[serde(
        rename = "org.matrix.msc4496.calendar_invite",
        alias = "m.calendar.invite"
    )]
    pub invite: CalendarInvite,
}

impl CalendarInviteEventContent {
    /// Creates the content for `invite`, generating the `m.text` fallback.
    pub fn new(invite: CalendarInvite) -> Self {
        Self {
            text: invite_text(&invite),
            invite,
        }
    }
}

string_enum! {
    /// iTIP (RFC 5546) scheduling method.
    pub enum InviteMethod {
        /// `REQUEST`: invite or update.
        Request = "REQUEST",
        /// `CANCEL`: the organiser cancels the event.
        Cancel = "CANCEL",
        /// `COUNTER`: an invitee proposes a change.
        Counter = "COUNTER",
    }
}

/// The `calendar_invite` content block.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CalendarInvite {
    /// The `uid` of the calendar event, preserved verbatim from iCalendar `UID` for bridges.
    pub uid: String,
    /// Revision number, incremented with every revision of the invite (iCalendar `SEQUENCE`).
    pub sequence: u32,
    /// Scheduling method.
    pub method: InviteMethod,
    /// Summary of the event being scheduled.
    pub calendar_event: InviteEventSummary,
}

impl CalendarInvite {
    /// An invite for `event` with the given method and sequence number.
    pub fn new(event: &CalendarEvent, method: InviteMethod, sequence: u32) -> Self {
        Self {
            uid: event.uid.clone(),
            sequence,
            method,
            calendar_event: event.into(),
        }
    }

    /// A `REQUEST` invite for `event` with the given sequence number.
    pub fn request(event: &CalendarEvent, sequence: u32) -> Self {
        Self::new(event, InviteMethod::Request, sequence)
    }
}

/// The reduced form of a [`CalendarEvent`] carried in an invite.
///
/// Optional fields are omitted from the JSON when absent, and `all_day` when `false`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InviteEventSummary {
    /// Event title.
    pub title: String,
    /// Start time.
    pub start: EventTime,
    /// End time.
    pub end: EventTime,
    /// The organiser's Matrix ID.
    ///
    /// Optional: when absent, the organiser is the sender of the invite. A `COUNTER` (sent by
    /// an invitee) must name the organiser, otherwise it cannot be attributed to the right
    /// event. Omitted from the JSON when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub organizer: Option<OwnedUserId>,
    /// Whether this is an all-day event.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub all_day: bool,
    /// Optional location.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<Location>,
    /// Optional MatrixRTC conference.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conference: Option<Conference>,
    /// Optional description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Optional recurrence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recurrence: Option<Recurrence>,
}

impl From<&CalendarEvent> for InviteEventSummary {
    fn from(ev: &CalendarEvent) -> Self {
        Self {
            title: ev.title.clone(),
            start: ev.start,
            end: ev.end,
            organizer: ev.organizer.clone(),
            all_day: ev.all_day,
            location: ev.location.clone(),
            conference: ev.conference.clone(),
            description: ev.description.clone(),
            recurrence: ev.recurrence.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono_tz::Europe::Berlin;
    use matrix_sdk::ruma::{owned_room_id, owned_server_name, owned_user_id};
    use serde_json::{Value, json};

    use super::*;

    fn msc_json() -> Value {
        json!({
          "type": "m.calendar.invite",
          "content": {
            "m.text": [
              {
                "body": "You are invited to: Team standup\n2026-07-01 09:00–09:30 (Europe/Berlin)\nOrganised by @alice:example.com",
                "mimetype": "text/plain"
              }
            ],
            "m.calendar.invite": {
              "uid": "550e8400-e29b-41d4-a716-446655440000",
              "sequence": 0,
              "method": "REQUEST",
              "calendar_event": {
                "title": "Team standup",
                "start": { "utc": "2026-07-01T07:00:00Z", "localtime": "2026-07-01T09:00:00", "timezone": "Europe/Berlin" },
                "end": { "utc": "2026-07-01T07:30:00Z", "localtime": "2026-07-01T09:30:00", "timezone": "Europe/Berlin" },
                "organizer": "@alice:example.com",
                "conference": { "type": "m.rtc", "application": "m.call", "room_id": "!callroom:example.com", "via": ["example.com"], "label": "Join call" }
              }
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
        ev.conference = Some(
            Conference::call(
                owned_room_id!("!callroom:example.com"),
                vec![owned_server_name!("example.com")],
            )
            .with_label("Join call"),
        );
        ev
    }

    #[test]
    fn msc_example_deserializes() {
        let v = msc_json();
        let content: CalendarInviteEventContent =
            serde_json::from_value(v["content"].clone()).unwrap();
        assert_eq!(content.invite, CalendarInvite::request(&standup(), 0));
        assert_eq!(content.invite.method, InviteMethod::Request);
        assert!(
            content
                .invite
                .calendar_event
                .conference
                .as_ref()
                .unwrap()
                .is_resolvable()
        );
    }

    #[test]
    fn builder_matches_msc_example() {
        let mut expected = msc_json()["content"].take();
        let obj = expected.as_object_mut().unwrap();
        let block = obj.remove("m.calendar.invite").unwrap();
        obj.insert("org.matrix.msc4496.calendar_invite".into(), block);

        let content = CalendarInviteEventContent::new(CalendarInvite::request(&standup(), 0));
        assert_eq!(serde_json::to_value(&content).unwrap(), expected);
    }

    #[test]
    fn cancel_and_counter_text() {
        let ev = standup();
        let cancel =
            CalendarInviteEventContent::new(CalendarInvite::new(&ev, InviteMethod::Cancel, 1));
        assert_eq!(
            cancel.text.find_plain(),
            Some(
                "Cancelled: Team standup\n2026-07-01 09:00–09:30 (Europe/Berlin)\nOrganised by @alice:example.com"
            )
        );
        let v = serde_json::to_value(&cancel).unwrap();
        assert_eq!(v["org.matrix.msc4496.calendar_invite"]["method"], "CANCEL");
        assert_eq!(v["org.matrix.msc4496.calendar_invite"]["sequence"], 1);
        let counter: CalendarInvite = serde_json::from_value(json!({
            "uid": "x", "sequence": 2, "method": "COUNTER",
            "calendar_event": {
                "title": "T", "start": { "date": "2026-12-25" }, "end": { "date": "2026-12-26" },
                "organizer": "@alice:example.com", "all_day": true
            }
        }))
        .unwrap();
        assert_eq!(counter.method, InviteMethod::Counter);
        assert!(counter.calendar_event.all_day);
    }
}
