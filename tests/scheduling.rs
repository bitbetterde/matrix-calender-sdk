//! Integration tests for invites, RSVPs and the scheduling index.

use chrono_tz::Europe::Berlin;
use matrix_calendar_sdk::{
    Error,
    events::{
        CalendarEvent, CalendarInvite, CalendarInviteEventContent, CalendarRsvp,
        CalendarRsvpEventContent, EventTime, InviteMethod, PartStat,
    },
    scheduling::{self, EventKey, InviteRecord, SchedulingIndex},
};
use matrix_sdk::{
    Room,
    deserialized_responses::TimelineEvent,
    ruma::{
        EventId, MilliSecondsSinceUnixEpoch, OwnedEventId, RoomId, UInt, UserId, event_id,
        events::{AnySyncTimelineEvent, MessageLikeEventType},
        owned_device_id, owned_user_id, room_id,
        serde::Raw,
        user_id,
    },
    test_utils::mocks::{MatrixMockServer, RoomMessagesResponseTemplate},
};
use matrix_sdk_test::event_factory::EventFactory;
use serde_json::{Value, json};

const UID: &str = "550e8400-e29b-41d4-a716-446655440000";
const INVITE_TYPE: &str = "org.matrix.msc4496.calendar_invite";
const RSVP_TYPE: &str = "org.matrix.msc4496.calendar_rsvp";

fn dm_room() -> &'static RoomId {
    room_id!("!dm:example.com")
}

fn alice() -> &'static UserId {
    user_id!("@alice:example.com")
}

fn bob() -> &'static UserId {
    user_id!("@bob:example.com")
}

fn carol() -> &'static UserId {
    user_id!("@carol:example.com")
}

fn standup() -> CalendarEvent {
    let mut ev = CalendarEvent::new(
        "Team standup",
        EventTime::timed("2026-07-01T09:00:00".parse().unwrap(), Berlin).unwrap(),
        EventTime::timed("2026-07-01T09:30:00".parse().unwrap(), Berlin).unwrap(),
        owned_user_id!("@alice:example.com"),
    );
    ev.uid = UID.into();
    ev
}

/// The `(organiser, uid)` key of Alice's standup.
fn key() -> EventKey {
    EventKey::new(alice().to_owned(), UID)
}

fn invite(sequence: u32, method: InviteMethod) -> CalendarInvite {
    CalendarInvite::new(&standup(), method, sequence)
}

fn ts(ms: u64) -> MilliSecondsSinceUnixEpoch {
    MilliSecondsSinceUnixEpoch(UInt::try_from(ms).unwrap())
}

fn invite_event(
    f: &EventFactory,
    id: &EventId,
    sender: &UserId,
    at: u64,
    invite: CalendarInvite,
) -> Raw<AnySyncTimelineEvent> {
    f.event(CalendarInviteEventContent::new(invite))
        .event_id(id)
        .sender(sender)
        .server_ts(at)
        .into_raw_sync()
}

fn rsvp_event(
    f: &EventFactory,
    id: &EventId,
    sender: &UserId,
    at: u64,
    invite_id: &EventId,
    rsvp: CalendarRsvp,
) -> Raw<AnySyncTimelineEvent> {
    f.event(CalendarRsvpEventContent::new(invite_id.to_owned(), rsvp))
        .event_id(id)
        .sender(sender)
        .server_ts(at)
        .into_raw_sync()
}

fn redacted_snapshot(raw: &Raw<AnySyncTimelineEvent>) -> Raw<AnySyncTimelineEvent> {
    let mut value: Value = serde_json::from_str(raw.json().get()).unwrap();
    value["content"] = json!({});
    value["unsigned"] = json!({"redacted_because": {"type": "m.room.redaction", "content": {}}});
    Raw::new(&value).unwrap().cast_unchecked()
}

#[test]
fn redacted_invite_snapshot_removes_invite_and_dependent_responses() {
    let f = EventFactory::new().room(dm_room());
    for stable in [false, true] {
        for backwards in [false, true] {
            let mut inv = invite_event(
                &f,
                event_id!("$inv"),
                alice(),
                1,
                invite(0, InviteMethod::Request),
            );
            if stable {
                inv = to_stable(inv);
            }
            let response = rsvp_event(
                &f,
                event_id!("$rsvp"),
                bob(),
                2,
                event_id!("$inv"),
                CalendarRsvp::new(UID, 0, PartStat::Accepted),
            );
            let redacted = redacted_snapshot(&inv);
            let mut index = SchedulingIndex::new();
            if backwards {
                assert!(!index.apply_raw_sync(&redacted).unwrap());
                assert!(!index.apply_raw_sync(&response).unwrap());
                assert!(!index.apply_raw_sync(&inv).unwrap());
            } else {
                assert!(index.apply_raw_sync(&inv).unwrap());
                assert!(index.apply_raw_sync(&response).unwrap());
                assert!(
                    index
                        .apply_timeline_event(&TimelineEvent::from_plaintext(redacted.clone()))
                        .unwrap()
                );
            }
            assert!(index.latest_invite(&key()).is_none());
            assert!(index.invites(&key()).is_empty());
            assert!(index.responses(&key()).is_empty());
            assert!(index.attendees(&key()).is_empty());
            assert_eq!(index.keys().count(), 0);
            assert_eq!(index.unresolved_responses(), 0);
            assert!(!index.apply_raw_sync(&redacted).unwrap());
            assert!(!index.apply_raw_sync(&inv).unwrap());
        }
    }
}

#[test]
fn redacted_rsvp_snapshot_reverts_to_previous_attendee_status() {
    let f = EventFactory::new().room(dm_room());
    for stable in [false, true] {
        let mut index = SchedulingIndex::new();
        index
            .apply_raw_sync(&invite_event(
                &f,
                event_id!("$inv"),
                alice(),
                1,
                invite(0, InviteMethod::Request),
            ))
            .unwrap();
        index
            .apply_raw_sync(&rsvp_event(
                &f,
                event_id!("$old"),
                bob(),
                2,
                event_id!("$inv"),
                CalendarRsvp::new(UID, 0, PartStat::Accepted),
            ))
            .unwrap();
        let mut response = rsvp_event(
            &f,
            event_id!("$new"),
            bob(),
            3,
            event_id!("$inv"),
            CalendarRsvp::new(UID, 0, PartStat::Declined),
        );
        if stable {
            response = to_stable(response);
        }
        index.apply_raw_sync(&response).unwrap();
        assert_eq!(index.attendees(&key())[0].partstat, PartStat::Declined);
        let redacted = redacted_snapshot(&response);
        assert!(index.apply_raw_sync(&redacted).unwrap());
        assert_eq!(index.responses(&key()).len(), 1);
        assert_eq!(index.attendees(&key())[0].partstat, PartStat::Accepted);
        assert!(!index.apply_raw_sync(&redacted).unwrap());
        assert!(!index.apply_raw_sync(&response).unwrap());
    }
}

#[test]
fn redacted_snapshots_remove_buffered_scheduling_events() {
    let f = EventFactory::new().room(dm_room());
    let inv = invite_event(
        &f,
        event_id!("$inv"),
        alice(),
        1,
        invite(0, InviteMethod::Request),
    );
    let response = rsvp_event(
        &f,
        event_id!("$rsvp"),
        bob(),
        2,
        event_id!("$inv"),
        CalendarRsvp::new(UID, 0, PartStat::Accepted),
    );
    for redact_invite in [false, true] {
        let mut index = SchedulingIndex::new();
        index.apply_raw_sync(&response).unwrap();
        assert_eq!(index.unresolved_responses(), 1);
        let redacted = redacted_snapshot(if redact_invite { &inv } else { &response });
        assert!(index.apply_raw_sync(&redacted).unwrap());
        assert_eq!(index.unresolved_responses(), 0);
        assert!(!index.apply_raw_sync(&redacted).unwrap());
        assert_eq!(index.apply_raw_sync(&inv).unwrap(), !redact_invite);
        assert!(index.attendees(&key()).is_empty());
        assert!(!index.apply_raw_sync(&response).unwrap());
    }
}

/// Rewrites an event built with the unstable identifiers to the stable ones.
fn to_stable(raw: Raw<AnySyncTimelineEvent>) -> Raw<AnySyncTimelineEvent> {
    let mut v: Value = serde_json::from_str(raw.json().get()).unwrap();
    let (ty, block) = match v["type"].as_str().unwrap() {
        INVITE_TYPE => ("m.calendar.invite", INVITE_TYPE),
        RSVP_TYPE => ("m.calendar.rsvp", RSVP_TYPE),
        other => panic!("unexpected type {other}"),
    };
    v["type"] = json!(ty);
    let content = v["content"].as_object_mut().unwrap();
    let payload = content.remove(block).unwrap();
    content.insert(ty.to_owned(), payload);
    Raw::from_json_string(v.to_string()).unwrap()
}

/// A joined DM, as seen by Alice (the organiser of the invites the tests send).
async fn joined_room(server: &MatrixMockServer) -> Room {
    joined_room_as(server, alice()).await
}

async fn joined_room_as(server: &MatrixMockServer, user: &UserId) -> Room {
    let client = server
        .client_builder()
        .logged_in_with_token("1234".into(), user.to_owned(), owned_device_id!("DEVICEID"))
        .build()
        .await;
    server.mock_room_state_encryption().plain().mount().await;
    server.sync_joined_room(&client, dm_room()).await
}

/// The JSON bodies of all `PUT /send/{event_type}/{txn}` requests for `event_type`.
async fn sent_bodies(server: &MatrixMockServer, event_type: &str) -> Vec<Value> {
    let needle = format!("/send/{event_type}/");
    server
        .server()
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.method.as_str() == "PUT" && r.url.path().contains(&needle))
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect()
}

#[tokio::test]
async fn send_invite_and_rsvp_hit_send_endpoints() {
    let server = MatrixMockServer::new().await;
    let room = joined_room(&server).await;

    server
        .mock_room_send()
        .for_type(MessageLikeEventType::from(INVITE_TYPE))
        .ok(event_id!("$invite"))
        .expect(1)
        .mount()
        .await;
    server
        .mock_room_send()
        .for_type(MessageLikeEventType::from(RSVP_TYPE))
        .ok(event_id!("$rsvp"))
        .expect(1)
        .mount()
        .await;

    let sent = scheduling::send_invite(&room, invite(0, InviteMethod::Request))
        .await
        .unwrap();
    assert_eq!(sent, "$invite");

    let rsvp = CalendarRsvp::new(UID, 0, PartStat::Accepted).with_comment("See you then!");
    let sent = scheduling::send_rsvp(&room, event_id!("$invite"), rsvp)
        .await
        .unwrap();
    assert_eq!(sent, "$rsvp");

    let invites = sent_bodies(&server, INVITE_TYPE).await;
    assert_eq!(invites.len(), 1);
    assert_eq!(
        invites[0],
        json!({
            "m.text": [{
                "body": "You are invited to: Team standup\n2026-07-01 09:00–09:30 (Europe/Berlin)\nOrganised by @alice:example.com",
                "mimetype": "text/plain"
            }],
            "org.matrix.msc4496.calendar_invite": {
                "uid": UID,
                "sequence": 0,
                "method": "REQUEST",
                "calendar_event": {
                    "title": "Team standup",
                    "start": { "utc": "2026-07-01T07:00:00Z", "localtime": "2026-07-01T09:00:00", "timezone": "Europe/Berlin" },
                    "end": { "utc": "2026-07-01T07:30:00Z", "localtime": "2026-07-01T09:30:00", "timezone": "Europe/Berlin" },
                    "organizer": "@alice:example.com"
                }
            }
        })
    );

    let rsvps = sent_bodies(&server, RSVP_TYPE).await;
    assert_eq!(rsvps.len(), 1);
    assert_eq!(
        rsvps[0],
        json!({
            "m.text": [{ "body": "Accepted the invitation", "mimetype": "text/plain" }],
            "org.matrix.msc4496.calendar_rsvp": {
                "uid": UID,
                "sequence": 0,
                "partstat": "ACCEPTED",
                "comment": "See you then!"
            },
            "m.relates_to": { "rel_type": "m.reference", "event_id": "$invite" }
        })
    );
}

#[tokio::test]
async fn cancel_and_counter_bodies() {
    let server = MatrixMockServer::new().await;
    let room = joined_room(&server).await;
    server
        .mock_room_send()
        .for_type(MessageLikeEventType::from(INVITE_TYPE))
        .ok(event_id!("$x"))
        .expect(2)
        .mount()
        .await;

    let previous = invite(2, InviteMethod::Request);
    scheduling::cancel_invite(&room, &previous).await.unwrap();

    // The previous invite names no organiser: its sender (Alice) is the organiser.
    let mut previous_record = InviteRecord {
        event_id: event_id!("$prev").to_owned(),
        sender: alice().to_owned(),
        origin_server_ts: ts(1),
        invite: previous.clone(),
    };
    previous_record.invite.calendar_event.organizer = None;
    let mut proposed = previous.calendar_event.clone();
    proposed.start = EventTime::timed("2026-07-01T10:00:00".parse().unwrap(), Berlin).unwrap();
    proposed.end = EventTime::timed("2026-07-01T10:30:00".parse().unwrap(), Berlin).unwrap();
    proposed.organizer = Some(owned_user_id!("@bob:example.com"));
    scheduling::counter_propose(&room, &previous_record, proposed)
        .await
        .unwrap();

    let bodies = sent_bodies(&server, INVITE_TYPE).await;
    assert_eq!(bodies.len(), 2);
    let cancel = &bodies[0][INVITE_TYPE];
    assert_eq!(cancel["method"], "CANCEL");
    assert_eq!(cancel["sequence"], 3);
    assert_eq!(cancel["uid"], UID);
    assert!(
        bodies[0]["m.text"][0]["body"]
            .as_str()
            .unwrap()
            .starts_with("Cancelled: Team standup")
    );

    let counter = &bodies[1][INVITE_TYPE];
    assert_eq!(counter["method"], "COUNTER");
    assert_eq!(counter["sequence"], 2);
    assert_eq!(counter["uid"], UID);
    assert_eq!(
        counter["calendar_event"]["start"]["utc"],
        "2026-07-01T08:00:00Z"
    );
    // The organiser cannot be changed by a counter-proposal.
    assert_eq!(counter["calendar_event"]["organizer"], "@alice:example.com");
}

#[test]
fn index_aggregates_responses_and_marks_stale() {
    let f = EventFactory::new().room(dm_room());
    let mut index = SchedulingIndex::new();

    let inv0 = invite_event(
        &f,
        event_id!("$inv0"),
        alice(),
        1000,
        invite(0, InviteMethod::Request),
    );
    assert!(index.apply_raw_sync(&inv0).unwrap());
    // Applying the same event twice is a no-op.
    assert!(!index.apply_raw_sync(&inv0).unwrap());

    let bob0 = rsvp_event(
        &f,
        event_id!("$bob0"),
        bob(),
        2000,
        event_id!("$inv0"),
        CalendarRsvp::new(UID, 0, PartStat::Tentative),
    );
    let carol0 = rsvp_event(
        &f,
        event_id!("$carol0"),
        carol(),
        3000,
        event_id!("$inv0"),
        CalendarRsvp::new(UID, 0, PartStat::Declined).with_comment("On holiday"),
    );
    assert!(index.apply_raw_sync(&bob0).unwrap());
    assert!(index.apply_raw_sync(&carol0).unwrap());

    // The organiser moves the event: sequence 1.
    let inv1 = invite_event(
        &f,
        event_id!("$inv1"),
        alice(),
        4000,
        invite(1, InviteMethod::Request),
    );
    assert!(index.apply_raw_sync(&inv1).unwrap());

    // Bob answers the new revision.
    let bob1 = rsvp_event(
        &f,
        event_id!("$bob1"),
        bob(),
        5000,
        event_id!("$inv1"),
        CalendarRsvp::new(UID, 1, PartStat::Accepted),
    );
    assert!(index.apply_raw_sync(&bob1).unwrap());

    // An RSVP whose uid does not match the referenced invite is ignored.
    let bogus = rsvp_event(
        &f,
        event_id!("$bogus"),
        user_id!("@mallory:example.com"),
        6000,
        event_id!("$inv1"),
        CalendarRsvp::new("other-uid", 1, PartStat::Accepted),
    );
    assert!(!index.apply_raw_sync(&bogus).unwrap());

    // Unrelated events are ignored.
    assert!(
        !index
            .apply_raw_sync(&f.text_msg("hi").sender(bob()).into_raw_sync())
            .unwrap()
    );

    assert_eq!(index.keys().cloned().collect::<Vec<_>>(), vec![key()]);
    let invites: Vec<_> = index
        .invites(&key())
        .iter()
        .map(|r| r.event_id.clone())
        .collect();
    assert_eq!(invites, vec![event_id!("$inv0"), event_id!("$inv1")]);
    let latest = index.latest_invite(&key()).unwrap();
    assert_eq!(latest.event_id, "$inv1");
    assert_eq!(latest.sender, alice());
    assert_eq!(latest.origin_server_ts, ts(4000));
    assert!(!index.is_cancelled(&key()));

    let responses = index.responses(&key());
    assert_eq!(responses.len(), 3);
    assert_eq!(responses[2].invite_event_id, "$inv1");

    let attendees = index.attendees(&key());
    assert_eq!(attendees.len(), 2);
    let bob_status = &attendees[0];
    assert_eq!(bob_status.user, bob());
    assert_eq!(bob_status.partstat, PartStat::Accepted);
    assert_eq!(bob_status.sequence, 1);
    assert_eq!(bob_status.responded_at, ts(5000));
    assert!(!bob_status.stale);
    let carol_status = &attendees[1];
    assert_eq!(carol_status.user, carol());
    assert_eq!(carol_status.partstat, PartStat::Declined);
    assert_eq!(carol_status.comment.as_deref(), Some("On holiday"));
    assert!(carol_status.stale);

    assert!(
        index
            .invites(&EventKey::new(alice().to_owned(), "unknown"))
            .is_empty()
    );
    assert!(
        index
            .attendees(&EventKey::new(alice().to_owned(), "unknown"))
            .is_empty()
    );
}

#[test]
fn rsvp_before_invite_is_buffered() {
    let f = EventFactory::new().room(dm_room());
    let mut index = SchedulingIndex::new();

    let rsvp = rsvp_event(
        &f,
        event_id!("$rsvp"),
        bob(),
        2000,
        event_id!("$inv"),
        CalendarRsvp::new(UID, 0, PartStat::Accepted),
    );
    assert!(index.apply_raw_sync(&rsvp).unwrap());
    assert!(index.responses(&key()).is_empty());
    assert!(index.attendees(&key()).is_empty());
    assert_eq!(index.unresolved_responses(), 1);

    let inv = invite_event(
        &f,
        event_id!("$inv"),
        alice(),
        1000,
        invite(0, InviteMethod::Request),
    );
    assert!(index.apply_raw_sync(&inv).unwrap());
    assert_eq!(index.unresolved_responses(), 0);
    let attendees = index.attendees(&key());
    assert_eq!(attendees.len(), 1);
    assert_eq!(attendees[0].user, bob());
    assert_eq!(attendees[0].partstat, PartStat::Accepted);

    // A buffered RSVP with the wrong uid is dropped on resolution.
    let mut index = SchedulingIndex::new();
    let wrong = rsvp_event(
        &f,
        event_id!("$wrong"),
        bob(),
        2000,
        event_id!("$inv"),
        CalendarRsvp::new("nope", 0, PartStat::Accepted),
    );
    index.apply_raw_sync(&wrong).unwrap();
    index.apply_raw_sync(&inv).unwrap();
    assert!(index.responses(&key()).is_empty());
    assert_eq!(index.unresolved_responses(), 0);
}

#[test]
fn cancel_and_counter_detection() {
    let f = EventFactory::new().room(dm_room());
    let mut index = SchedulingIndex::new();

    index
        .apply_raw_sync(&invite_event(
            &f,
            event_id!("$inv0"),
            alice(),
            1000,
            invite(0, InviteMethod::Request),
        ))
        .unwrap();
    // A counter-proposal from bob is not an organiser revision.
    index
        .apply_raw_sync(&invite_event(
            &f,
            event_id!("$counter"),
            bob(),
            2000,
            invite(0, InviteMethod::Counter),
        ))
        .unwrap();
    assert_eq!(index.latest_invite(&key()).unwrap().event_id, "$inv0");
    assert_eq!(index.invites(&key()).len(), 2);
    assert!(!index.is_cancelled(&key()));

    index
        .apply_raw_sync(&invite_event(
            &f,
            event_id!("$cancel"),
            alice(),
            3000,
            invite(1, InviteMethod::Cancel),
        ))
        .unwrap();
    assert_eq!(index.latest_invite(&key()).unwrap().event_id, "$cancel");
    assert!(index.is_cancelled(&key()));

    // With only a counter-proposal, it is the latest invite.
    let mut only_counter = SchedulingIndex::new();
    only_counter
        .apply_raw_sync(&invite_event(
            &f,
            event_id!("$c"),
            bob(),
            1,
            invite(0, InviteMethod::Counter),
        ))
        .unwrap();
    assert_eq!(only_counter.latest_invite(&key()).unwrap().event_id, "$c");
}

#[test]
fn stable_and_unstable_type_names() {
    let f = EventFactory::new().room(dm_room());
    let mut index = SchedulingIndex::new();

    let inv = to_stable(invite_event(
        &f,
        event_id!("$inv"),
        alice(),
        1000,
        invite(0, InviteMethod::Request),
    ));
    assert!(inv.json().get().contains("\"m.calendar.invite\""));
    assert!(index.apply_raw_sync(&inv).unwrap());

    let stable_rsvp = to_stable(rsvp_event(
        &f,
        event_id!("$r1"),
        bob(),
        2000,
        event_id!("$inv"),
        CalendarRsvp::new(UID, 0, PartStat::Accepted),
    ));
    let unstable_rsvp = rsvp_event(
        &f,
        event_id!("$r2"),
        carol(),
        3000,
        event_id!("$inv"),
        CalendarRsvp::new(UID, 0, PartStat::Declined),
    );
    assert!(index.apply_raw_sync(&stable_rsvp).unwrap());
    assert!(
        index
            .apply_timeline_event(&TimelineEvent::from_plaintext(unstable_rsvp))
            .unwrap()
    );

    assert_eq!(index.latest_invite(&key()).unwrap().event_id, "$inv");
    let partstats: Vec<_> = index
        .attendees(&key())
        .into_iter()
        .map(|a| a.partstat)
        .collect();
    assert_eq!(partstats, vec![PartStat::Accepted, PartStat::Declined]);
}

#[test]
fn redactions_and_malformed_events() {
    let f = EventFactory::new().room(dm_room());
    let mut index = SchedulingIndex::new();

    // An already redacted invite is dropped.
    let redacted: Raw<AnySyncTimelineEvent> = Raw::from_json_string(
        json!({
            "type": INVITE_TYPE,
            "event_id": "$redacted",
            "sender": "@alice:example.com",
            "origin_server_ts": 1,
            "content": {},
            "unsigned": { "redacted_because": { "type": "m.room.redaction", "content": {} } }
        })
        .to_string(),
    )
    .unwrap();
    assert!(!index.apply_raw_sync(&redacted).unwrap());

    // A malformed invite is skipped.
    let malformed: Raw<AnySyncTimelineEvent> = Raw::from_json_string(
        json!({
            "type": INVITE_TYPE,
            "event_id": "$malformed",
            "sender": "@alice:example.com",
            "origin_server_ts": 1,
            "content": { "org.matrix.msc4496.calendar_invite": { "uid": 5 } }
        })
        .to_string(),
    )
    .unwrap();
    assert!(!index.apply_raw_sync(&malformed).unwrap());

    // An RSVP with a relation other than m.reference is skipped.
    let wrong_rel: Raw<AnySyncTimelineEvent> = Raw::from_json_string(
        json!({
            "type": RSVP_TYPE,
            "event_id": "$wrongrel",
            "sender": "@bob:example.com",
            "origin_server_ts": 1,
            "content": {
                "org.matrix.msc4496.calendar_rsvp": { "uid": UID, "sequence": 0, "partstat": "ACCEPTED" },
                "m.relates_to": { "rel_type": "m.annotation", "event_id": "$inv", "key": "x" }
            }
        })
        .to_string(),
    )
    .unwrap();
    assert!(!index.apply_raw_sync(&wrong_rel).unwrap());
    assert_eq!(index.keys().count(), 0);

    // A redaction removes an RSVP that has been applied.
    index
        .apply_raw_sync(&invite_event(
            &f,
            event_id!("$inv"),
            alice(),
            1000,
            invite(0, InviteMethod::Request),
        ))
        .unwrap();
    index
        .apply_raw_sync(&rsvp_event(
            &f,
            event_id!("$rsvp"),
            bob(),
            2000,
            event_id!("$inv"),
            CalendarRsvp::new(UID, 0, PartStat::Accepted),
        ))
        .unwrap();
    assert_eq!(index.attendees(&key()).len(), 1);
    let redaction: Raw<AnySyncTimelineEvent> = Raw::from_json_string(
        json!({
            "type": "m.room.redaction",
            "event_id": "$redaction",
            "sender": "@bob:example.com",
            "origin_server_ts": 3000,
            "redacts": "$rsvp",
            "content": { "redacts": "$rsvp" }
        })
        .to_string(),
    )
    .unwrap();
    assert!(index.apply_raw_sync(&redaction).unwrap());
    assert!(index.attendees(&key()).is_empty());
    assert_eq!(index.invites(&key()).len(), 1);
}

#[tokio::test]
async fn respond_fetches_invite_and_sends_rsvp() {
    let server = MatrixMockServer::new().await;
    let room = joined_room(&server).await;
    let f = EventFactory::new().room(dm_room());

    server
        .mock_room_event()
        .match_event_id()
        .ok(f
            .event(CalendarInviteEventContent::new(invite(
                4,
                InviteMethod::Request,
            )))
            .event_id(event_id!("$invite"))
            .sender(alice())
            .into_event())
        .expect(1)
        .mount()
        .await;
    server
        .mock_room_send()
        .for_type(MessageLikeEventType::from(RSVP_TYPE))
        .ok(event_id!("$rsvp"))
        .expect(1)
        .mount()
        .await;

    let sent = scheduling::respond(
        &room,
        event_id!("$invite"),
        PartStat::Accepted,
        Some("See you then!".into()),
    )
    .await
    .unwrap();
    assert_eq!(sent, "$rsvp");

    let bodies = sent_bodies(&server, RSVP_TYPE).await;
    assert_eq!(
        bodies,
        vec![json!({
            "m.text": [{ "body": "Accepted: Team standup on 2026-07-01", "mimetype": "text/plain" }],
            "org.matrix.msc4496.calendar_rsvp": {
                "uid": UID,
                "sequence": 4,
                "partstat": "ACCEPTED",
                "comment": "See you then!"
            },
            "m.relates_to": { "rel_type": "m.reference", "event_id": "$invite" }
        })]
    );
}

#[tokio::test]
async fn respond_rejects_non_invites() {
    let server = MatrixMockServer::new().await;
    let room = joined_room(&server).await;
    let f = EventFactory::new().room(dm_room());

    server
        .mock_room_event()
        .match_event_id()
        .ok(f
            .text_msg("not an invite")
            .event_id(event_id!("$text"))
            .sender(alice())
            .into_event())
        .mount()
        .await;

    let err = scheduling::respond(&room, event_id!("$text"), PartStat::Declined, None)
        .await
        .unwrap_err();
    assert!(
        matches!(err, matrix_calendar_sdk::Error::InvalidIdentifier(_)),
        "{err:?}"
    );
}

#[tokio::test]
async fn load_paginates_backwards() {
    let server = MatrixMockServer::new().await;
    let room = joined_room(&server).await;
    let f = EventFactory::new().room(dm_room());

    let inv0 = f
        .event(CalendarInviteEventContent::new(invite(
            0,
            InviteMethod::Request,
        )))
        .event_id(event_id!("$inv0"))
        .sender(alice())
        .server_ts(1000u64);
    let bob0 = f
        .event(CalendarRsvpEventContent::new(
            OwnedEventId::from(event_id!("$inv0")),
            CalendarRsvp::new(UID, 0, PartStat::Accepted),
        ))
        .event_id(event_id!("$bob0"))
        .sender(bob())
        .server_ts(2000u64);
    let inv1 = f
        .event(CalendarInviteEventContent::new(invite(
            1,
            InviteMethod::Request,
        )))
        .event_id(event_id!("$inv1"))
        .sender(alice())
        .server_ts(3000u64);
    let carol1 = f
        .event(CalendarRsvpEventContent::new(
            OwnedEventId::from(event_id!("$inv1")),
            CalendarRsvp::new(UID, 1, PartStat::Tentative),
        ))
        .event_id(event_id!("$carol1"))
        .sender(carol())
        .server_ts(4000u64);

    // Backward pagination: newest first, over two pages.
    server
        .mock_room_messages()
        .match_from("page2")
        .ok(RoomMessagesResponseTemplate::default()
            .events(vec![bob0.into_raw_timeline(), inv0.into_raw_timeline()]))
        .mock_once()
        .mount()
        .await;
    server
        .mock_room_messages()
        .ok(RoomMessagesResponseTemplate::default()
            .events(vec![carol1.into_raw_timeline(), inv1.into_raw_timeline()])
            .end_token("page2"))
        .mock_once()
        .mount()
        .await;

    let index = SchedulingIndex::load(&room, None).await.unwrap();
    assert_eq!(index.latest_invite(&key()).unwrap().event_id, "$inv1");
    assert_eq!(index.unresolved_responses(), 0);
    let attendees = index.attendees(&key());
    assert_eq!(attendees.len(), 2);
    assert_eq!(attendees[0].user, bob());
    assert!(attendees[0].stale);
    assert_eq!(attendees[1].user, carol());
    assert_eq!(attendees[1].partstat, PartStat::Tentative);
    assert!(!attendees[1].stale);
}

fn mallory() -> &'static UserId {
    user_id!("@mallory:example.com")
}

/// An invite for the standup that names `organizer` (or none).
fn invite_by(organizer: Option<&UserId>, sequence: u32, method: InviteMethod) -> CalendarInvite {
    let mut invite = invite(sequence, method);
    invite.calendar_event.organizer = organizer.map(ToOwned::to_owned);
    invite
}

#[test]
fn spoofed_cancel_and_request_are_ignored() {
    let f = EventFactory::new().room(dm_room());
    let mut index = SchedulingIndex::new();

    // Alice invites, Bob accepts.
    assert!(
        index
            .apply_raw_sync(&invite_event(
                &f,
                event_id!("$inv0"),
                alice(),
                1000,
                invite(0, InviteMethod::Request),
            ))
            .unwrap()
    );
    assert!(
        index
            .apply_raw_sync(&rsvp_event(
                &f,
                event_id!("$bob0"),
                bob(),
                2000,
                event_id!("$inv0"),
                CalendarRsvp::new(UID, 0, PartStat::Accepted),
            ))
            .unwrap()
    );

    // Mallory sends a CANCEL with Alice's uid, naming Alice as organiser and a much higher
    // sequence: ignored.
    let spoofed_cancel = invite_event(
        &f,
        event_id!("$spoof-cancel"),
        mallory(),
        3000,
        invite_by(Some(alice()), 99, InviteMethod::Cancel),
    );
    assert!(!index.apply_raw_sync(&spoofed_cancel).unwrap());
    // A REQUEST naming Alice is ignored too.
    let spoofed_request = invite_event(
        &f,
        event_id!("$spoof-request"),
        mallory(),
        3500,
        invite_by(Some(alice()), 100, InviteMethod::Request),
    );
    assert!(!index.apply_raw_sync(&spoofed_request).unwrap());

    assert!(!index.is_cancelled(&key()));
    let latest = index.latest_invite(&key()).unwrap();
    assert_eq!(latest.event_id, "$inv0");
    assert_eq!(index.invites(&key()).len(), 1);
    let attendees = index.attendees(&key());
    assert_eq!(attendees.len(), 1);
    assert_eq!(attendees[0].user, bob());
    assert!(!attendees[0].stale);

    // RSVPs to a spoofed invite are dropped, before or after it arrives.
    let early_rsvp = rsvp_event(
        &f,
        event_id!("$carol-early"),
        carol(),
        3600,
        event_id!("$spoof-later"),
        CalendarRsvp::new(UID, 101, PartStat::Accepted),
    );
    assert!(index.apply_raw_sync(&early_rsvp).unwrap());
    assert_eq!(index.unresolved_responses(), 1);
    index
        .apply_raw_sync(&invite_event(
            &f,
            event_id!("$spoof-later"),
            mallory(),
            3700,
            invite_by(Some(alice()), 101, InviteMethod::Request),
        ))
        .unwrap();
    assert_eq!(index.unresolved_responses(), 0);
    assert!(
        !index
            .apply_raw_sync(&rsvp_event(
                &f,
                event_id!("$carol-late"),
                carol(),
                3800,
                event_id!("$spoof-request"),
                CalendarRsvp::new(UID, 100, PartStat::Accepted),
            ))
            .unwrap()
    );
    assert_eq!(index.unresolved_responses(), 0);
    assert_eq!(index.attendees(&key()).len(), 1);

    // Mallory's own REQUEST with Alice's uid is a separate event under Mallory's key, whether
    // it names Mallory or no organiser at all.
    for (id, organizer) in [
        (event_id!("$mallory-named"), Some(mallory())),
        (event_id!("$mallory-implicit"), None),
    ] {
        assert!(
            index
                .apply_raw_sync(&invite_event(
                    &f,
                    id,
                    mallory(),
                    4000,
                    invite_by(organizer, 7, InviteMethod::Request),
                ))
                .unwrap()
        );
    }
    let mallory_key = EventKey::new(mallory().to_owned(), UID);
    assert_eq!(
        index.keys().cloned().collect::<Vec<_>>(),
        vec![key(), mallory_key.clone()]
    );
    assert_eq!(index.invites(&mallory_key).len(), 2);
    assert_eq!(
        index.latest_invite(&mallory_key).unwrap().invite.sequence,
        7
    );
    assert_eq!(index.latest_invite(&key()).unwrap().event_id, "$inv0");
    // Mallory can cancel her own event, which leaves Alice's untouched.
    index
        .apply_raw_sync(&invite_event(
            &f,
            event_id!("$mallory-cancel"),
            mallory(),
            5000,
            invite_by(None, 8, InviteMethod::Cancel),
        ))
        .unwrap();
    assert!(index.is_cancelled(&mallory_key));
    assert!(!index.is_cancelled(&key()));
}

#[test]
fn counters_are_filed_under_the_named_organiser() {
    let f = EventFactory::new().room(dm_room());
    let mut index = SchedulingIndex::new();

    // Alice's invite names no organiser: she is the organiser as its sender.
    index
        .apply_raw_sync(&invite_event(
            &f,
            event_id!("$inv"),
            alice(),
            1000,
            invite_by(None, 0, InviteMethod::Request),
        ))
        .unwrap();
    // Bob's counter names Alice and is filed under her key.
    assert!(
        index
            .apply_raw_sync(&invite_event(
                &f,
                event_id!("$counter"),
                bob(),
                2000,
                invite_by(Some(alice()), 0, InviteMethod::Counter),
            ))
            .unwrap()
    );
    let invites = index.invites(&key());
    assert_eq!(invites.len(), 2);
    assert_eq!(invites[1].sender, bob());
    assert_eq!(invites[1].organizer(), alice());
    assert_eq!(index.latest_invite(&key()).unwrap().event_id, "$inv");

    // A counter without an organiser cannot be attributed and is ignored.
    assert!(
        !index
            .apply_raw_sync(&invite_event(
                &f,
                event_id!("$anon-counter"),
                carol(),
                3000,
                invite_by(None, 0, InviteMethod::Counter),
            ))
            .unwrap()
    );
    assert_eq!(index.keys().count(), 1);
    assert_eq!(index.invites(&key()).len(), 2);
}

#[test]
fn redacting_an_invite_drops_its_rsvps() {
    let f = EventFactory::new().room(dm_room());
    let mut index = SchedulingIndex::new();
    for (id, at, sequence) in [(event_id!("$inv0"), 1000, 0), (event_id!("$inv1"), 3000, 1)] {
        index
            .apply_raw_sync(&invite_event(
                &f,
                id,
                alice(),
                at,
                invite(sequence, InviteMethod::Request),
            ))
            .unwrap();
    }
    for (id, user, at, invite_id, sequence) in [
        (event_id!("$bob0"), bob(), 2000, event_id!("$inv0"), 0),
        (event_id!("$carol1"), carol(), 4000, event_id!("$inv1"), 1),
    ] {
        index
            .apply_raw_sync(&rsvp_event(
                &f,
                id,
                user,
                at,
                invite_id,
                CalendarRsvp::new(UID, sequence, PartStat::Accepted),
            ))
            .unwrap();
    }
    assert_eq!(index.attendees(&key()).len(), 2);

    let redaction = |id: &str, redacts: &str| -> Raw<AnySyncTimelineEvent> {
        Raw::from_json_string(
            json!({
                "type": "m.room.redaction",
                "event_id": id,
                "sender": "@alice:example.com",
                "origin_server_ts": 5000,
                "content": { "redacts": redacts }
            })
            .to_string(),
        )
        .unwrap()
    };
    assert!(index.apply_raw_sync(&redaction("$red0", "$inv0")).unwrap());
    let attendees = index.attendees(&key());
    assert_eq!(attendees.len(), 1);
    assert_eq!(attendees[0].user, carol());
    assert_eq!(index.responses(&key()).len(), 1);
    assert_eq!(index.invites(&key()).len(), 1);

    // A late RSVP to the redacted invite is dropped instead of being buffered forever.
    assert!(
        !index
            .apply_raw_sync(&rsvp_event(
                &f,
                event_id!("$dave0"),
                user_id!("@dave:example.com"),
                6000,
                event_id!("$inv0"),
                CalendarRsvp::new(UID, 0, PartStat::Declined),
            ))
            .unwrap()
    );
    assert_eq!(index.unresolved_responses(), 0);

    // Redacting the last invite removes the key with all its responses.
    index.apply_raw_sync(&redaction("$red1", "$inv1")).unwrap();
    assert!(index.attendees(&key()).is_empty());
    assert_eq!(index.keys().count(), 0);

    // Backward pagination: the redaction arrives before the invite and its RSVP.
    let mut index = SchedulingIndex::new();
    index.apply_raw_sync(&redaction("$red0", "$inv0")).unwrap();
    index
        .apply_raw_sync(&rsvp_event(
            &f,
            event_id!("$bob0"),
            bob(),
            2000,
            event_id!("$inv0"),
            CalendarRsvp::new(UID, 0, PartStat::Accepted),
        ))
        .unwrap();
    index
        .apply_raw_sync(&invite_event(
            &f,
            event_id!("$inv0"),
            alice(),
            1000,
            invite(0, InviteMethod::Request),
        ))
        .unwrap();
    assert_eq!(index.keys().count(), 0);
    assert_eq!(index.unresolved_responses(), 0);
}

#[test]
fn unknown_methods_and_partstats_round_trip() {
    let content: CalendarInviteEventContent = serde_json::from_value(json!({
        "org.matrix.msc4496.calendar_invite": {
            "uid": UID, "sequence": 0, "method": "REFRESH",
            "calendar_event": {
                "title": "T", "start": { "date": "2026-12-25" }, "end": { "date": "2026-12-26" },
                "all_day": true
            }
        }
    }))
    .unwrap();
    assert_eq!(content.invite.method, InviteMethod::Other("REFRESH".into()));
    assert_eq!(content.invite.calendar_event.organizer, None);
    let v = serde_json::to_value(&content.invite).unwrap();
    assert_eq!(v["method"], "REFRESH");
    assert!(v["calendar_event"].get("organizer").is_none());

    let rsvp: CalendarRsvp = serde_json::from_value(json!({
        "uid": UID, "sequence": 0, "partstat": "DELEGATED"
    }))
    .unwrap();
    assert_eq!(rsvp.partstat, PartStat::Other("DELEGATED".into()));
    assert_eq!(
        serde_json::to_value(&rsvp).unwrap()["partstat"],
        "DELEGATED"
    );

    // The fallback text omits the organiser line when there is none.
    let text = CalendarInviteEventContent::new(invite_by(None, 0, InviteMethod::Request)).text;
    assert_eq!(
        text.find_plain(),
        Some("You are invited to: Team standup\n2026-07-01 09:00–09:30 (Europe/Berlin)")
    );
}

#[tokio::test]
async fn respond_rejects_spoofed_invites() {
    let server = MatrixMockServer::new().await;
    let room = joined_room(&server).await;
    let f = EventFactory::new().room(dm_room());

    server
        .mock_room_event()
        .match_event_id()
        .ok(f
            .event(CalendarInviteEventContent::new(invite_by(
                Some(alice()),
                5,
                InviteMethod::Request,
            )))
            .event_id(event_id!("$spoof"))
            .sender(mallory())
            .into_event())
        .mount()
        .await;

    let err = scheduling::respond(&room, event_id!("$spoof"), PartStat::Accepted, None)
        .await
        .unwrap_err();
    assert!(
        matches!(err, matrix_calendar_sdk::Error::PermissionDenied(_)),
        "{err:?}"
    );
    assert!(sent_bodies(&server, RSVP_TYPE).await.is_empty());
}

#[tokio::test]
async fn invites_for_another_organiser_are_not_sent() {
    let server = MatrixMockServer::new().await;
    // Mallory is in the room, but Alice is the organiser of the standup.
    let room = joined_room_as(&server, mallory()).await;
    server
        .mock_room_send()
        .for_type(MessageLikeEventType::from(INVITE_TYPE))
        .ok(event_id!("$x"))
        .expect(1)
        .mount()
        .await;

    let previous = invite(0, InviteMethod::Request);
    assert!(matches!(
        scheduling::cancel_invite(&room, &previous).await,
        Err(Error::PermissionDenied(_))
    ));
    assert!(matches!(
        scheduling::send_invite(&room, invite(5, InviteMethod::Request)).await,
        Err(Error::PermissionDenied(_))
    ));

    // A COUNTER names the organiser by design and is sent; so is an invite that names no
    // organiser (the sender is then the organiser).
    let record = InviteRecord {
        event_id: event_id!("$prev").to_owned(),
        sender: alice().to_owned(),
        origin_server_ts: ts(1),
        invite: previous.clone(),
    };
    scheduling::counter_propose(&room, &record, previous.calendar_event.clone())
        .await
        .unwrap();
    let bodies = sent_bodies(&server, INVITE_TYPE).await;
    assert_eq!(bodies.len(), 1);
    assert_eq!(bodies[0][INVITE_TYPE]["method"], "COUNTER");
}
