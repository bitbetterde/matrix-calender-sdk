//! The `calendar_rsvp` message-like event (MSC4496 §3.2).

use matrix_sdk::ruma::{
    OwnedEventId,
    events::{macros::EventContent, message::TextContentBlock, relation::Reference},
};
use serde::{Deserialize, Serialize};

use super::{
    invite::{CalendarInvite, InviteEventSummary},
    text::{rsvp_text, serialize_text_block},
};

/// Content of the `org.matrix.msc4496.calendar_rsvp` (stable: `m.calendar.rsvp`) timeline
/// event, sent by an invitee in reply to an invite.
#[derive(Clone, Debug, Serialize, Deserialize, EventContent)]
#[ruma_event(
    type = "org.matrix.msc4496.calendar_rsvp",
    alias = "m.calendar.rsvp",
    kind = MessageLike
)]
pub struct CalendarRsvpEventContent {
    /// Human-readable fallback.
    #[serde(
        rename = "m.text",
        alias = "org.matrix.msc1767.text",
        default,
        skip_serializing_if = "TextContentBlock::is_empty",
        serialize_with = "serialize_text_block"
    )]
    pub text: TextContentBlock,

    /// The structured response.
    #[serde(rename = "org.matrix.msc4496.calendar_rsvp", alias = "m.calendar.rsvp")]
    pub rsvp: CalendarRsvp,

    /// `m.reference` relation to the invite event.
    #[serde(rename = "m.relates_to")]
    pub relates_to: Reference,
}

impl CalendarRsvpEventContent {
    /// Creates an RSVP to the invite `invite_event_id` with a generic `m.text` fallback such
    /// as `Accepted the invitation`.
    pub fn new(invite_event_id: OwnedEventId, rsvp: CalendarRsvp) -> Self {
        Self {
            text: rsvp_text(&rsvp.partstat, None),
            rsvp,
            relates_to: Reference::new(invite_event_id),
        }
    }

    /// Creates an RSVP whose `m.text` names the event, e.g.
    /// `Accepted: Team standup on 2026-07-01`.
    pub fn with_summary(
        invite_event_id: OwnedEventId,
        rsvp: CalendarRsvp,
        summary: &InviteEventSummary,
    ) -> Self {
        Self {
            text: rsvp_text(&rsvp.partstat, Some(summary)),
            rsvp,
            relates_to: Reference::new(invite_event_id),
        }
    }

    /// The event ID of the invite this RSVP responds to.
    pub fn invite_event_id(&self) -> &OwnedEventId {
        &self.relates_to.event_id
    }
}

string_enum! {
    /// iTIP participation status (`PARTSTAT`).
    pub enum PartStat {
        /// `ACCEPTED`
        Accepted = "ACCEPTED",
        /// `DECLINED`
        Declined = "DECLINED",
        /// `TENTATIVE`
        Tentative = "TENTATIVE",
        /// `NEEDS-ACTION`
        NeedsAction = "NEEDS-ACTION",
    }
}

/// The `calendar_rsvp` content block.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CalendarRsvp {
    /// The `uid` of the calendar event.
    pub uid: String,
    /// The `sequence` of the invite being answered.
    pub sequence: u32,
    /// The participation status.
    pub partstat: PartStat,
    /// Optional free-text comment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
}

impl CalendarRsvp {
    /// Creates a response with the given fields and no comment.
    pub fn new(uid: impl Into<String>, sequence: u32, partstat: PartStat) -> Self {
        Self {
            uid: uid.into(),
            sequence,
            partstat,
            comment: None,
        }
    }

    /// Creates a response to `invite` (copying its `uid` and `sequence`).
    pub fn for_invite(invite: &CalendarInvite, partstat: PartStat) -> Self {
        Self::new(invite.uid.clone(), invite.sequence, partstat)
    }

    /// Sets the comment.
    pub fn with_comment(mut self, comment: impl Into<String>) -> Self {
        self.comment = Some(comment.into());
        self
    }
}

#[cfg(test)]
mod tests {
    use chrono_tz::Europe::Berlin;
    use matrix_sdk::ruma::{owned_event_id, owned_user_id};
    use serde_json::{Value, json};

    use super::*;
    use crate::events::{CalendarEvent, EventTime};

    fn msc_json() -> Value {
        json!({
          "type": "m.calendar.rsvp",
          "content": {
            "m.text": [{ "body": "Accepted: Team standup on 2026-07-01", "mimetype": "text/plain" }],
            "m.calendar.rsvp": {
              "uid": "550e8400-e29b-41d4-a716-446655440000",
              "sequence": 0,
              "partstat": "ACCEPTED",
              "comment": "See you then!"
            },
            "m.relates_to": {
              "rel_type": "m.reference",
              "event_id": "$invite_event_id"
            }
          }
        })
    }

    #[test]
    fn msc_example_deserializes() {
        let content: CalendarRsvpEventContent =
            serde_json::from_value(msc_json()["content"].clone()).unwrap();
        assert_eq!(content.rsvp.partstat, PartStat::Accepted);
        assert_eq!(content.rsvp.comment.as_deref(), Some("See you then!"));
        assert_eq!(content.invite_event_id(), "$invite_event_id");
    }

    #[test]
    fn builder_matches_msc_example() {
        let mut expected = msc_json()["content"].take();
        let obj = expected.as_object_mut().unwrap();
        let block = obj.remove("m.calendar.rsvp").unwrap();
        obj.insert("org.matrix.msc4496.calendar_rsvp".into(), block);

        let mut ev = CalendarEvent::new(
            "Team standup",
            EventTime::timed("2026-07-01T09:00:00".parse().unwrap(), Berlin).unwrap(),
            EventTime::timed("2026-07-01T09:30:00".parse().unwrap(), Berlin).unwrap(),
            owned_user_id!("@alice:example.com"),
        );
        ev.uid = "550e8400-e29b-41d4-a716-446655440000".into();
        let invite = CalendarInvite::request(&ev, 0);
        let content = CalendarRsvpEventContent::with_summary(
            owned_event_id!("$invite_event_id"),
            CalendarRsvp::for_invite(&invite, PartStat::Accepted).with_comment("See you then!"),
            &invite.calendar_event,
        );
        assert_eq!(serde_json::to_value(&content).unwrap(), expected);
    }

    #[test]
    fn partstat_strings_and_generic_text() {
        assert_eq!(
            serde_json::to_value(PartStat::NeedsAction).unwrap(),
            json!("NEEDS-ACTION")
        );
        let content = CalendarRsvpEventContent::new(
            owned_event_id!("$i"),
            CalendarRsvp::new("u", 3, PartStat::Declined),
        );
        assert_eq!(content.text.find_plain(), Some("Declined the invitation"));
        let v = serde_json::to_value(&content).unwrap();
        assert!(
            v["org.matrix.msc4496.calendar_rsvp"]
                .get("comment")
                .is_none()
        );
        assert_eq!(
            v["m.relates_to"],
            json!({ "rel_type": "m.reference", "event_id": "$i" })
        );
    }
}
