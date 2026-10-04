//! Tests for the high-level calendar room API and the in-memory `CalendarIndex`.

use chrono::{DateTime, NaiveDateTime, TimeZone, Utc};
use chrono_tz::Europe::Berlin;
use matrix_calendar_sdk::{
    Error,
    calendar::{
        Calendar, CalendarIndex, CreateCalendarOptions, EventKey, LoadOptions, list_calendars,
    },
    events::{
        CalendarEvent, CalendarEventEventContent, CalendarInfo, EventTime, Frequency, RRule,
        Recurrence, Status,
    },
    ids,
};
use matrix_sdk::{
    ruma::{
        EventId, MilliSecondsSinceUnixEpoch, OwnedEventId, RoomId, UserId, event_id,
        events::{AnySyncStateEvent, AnySyncTimelineEvent, AnyTimelineEvent, MessageLikeEventType},
        owned_event_id, room_id,
        serde::Raw,
        user_id,
    },
    test_utils::mocks::{MatrixMockServer, RoomMessagesResponseTemplate},
};
use matrix_sdk_test::{JoinedRoomBuilder, event_factory::EventFactory};
use serde_json::{Value, json};

const ALICE: &str = "@alice:example.org";
const BOB: &str = "@bob:example.org";
/// The user the mock client is logged in as.
const OWN: &str = "@example:localhost";

fn alice() -> &'static UserId {
    user_id!("@alice:example.org")
}

fn bob() -> &'static UserId {
    user_id!("@bob:example.org")
}

fn local(s: &str) -> NaiveDateTime {
    s.parse().expect("valid local datetime")
}

fn utc(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s)
        .expect("valid RFC 3339")
        .with_timezone(&Utc)
}

fn event(uid: &str, title: &str, organizer: &UserId, start: &str, end: &str) -> CalendarEvent {
    let mut ev = CalendarEvent::new(
        title,
        EventTime::timed(local(start), Berlin).expect("valid start"),
        EventTime::timed(local(end), Berlin).expect("valid end"),
        organizer.to_owned(),
    );
    ev.uid = uid.to_owned();
    ev
}

fn standup(organizer: &UserId) -> CalendarEvent {
    event(
        "standup-uid",
        "Team standup",
        organizer,
        "2026-07-01T09:00:00",
        "2026-07-01T09:30:00",
    )
}

fn weekly_standup(organizer: &UserId) -> CalendarEvent {
    let mut ev = standup(organizer);
    ev.recurrence = Some(Recurrence::new(RRule::new(Frequency::Weekly)));
    ev
}

/// A sync-format calendar event with an explicit `type`, so that the stable name can be used.
fn raw_sync(
    event_type: &str,
    event_id: &str,
    sender: &str,
    ts: u64,
    content: Value,
) -> Raw<AnySyncTimelineEvent> {
    Raw::new(&json!({
        "type": event_type,
        "event_id": event_id,
        "sender": sender,
        "origin_server_ts": ts,
        "content": content,
    }))
    .expect("serializable")
    .cast_unchecked()
}

fn content_json(content: CalendarEventEventContent) -> Value {
    serde_json::to_value(content).expect("serializable")
}

/// Rewrites unstable block and rel_type names in `v` to the stable ones.
fn to_stable(v: Value) -> Value {
    let s = serde_json::to_string(&v)
        .expect("serializable")
        .replace(ids::CALENDAR_EVENT_UNSTABLE, ids::CALENDAR_EVENT)
        .replace(
            ids::CALENDAR_OVERRIDE_REL_TYPE_UNSTABLE,
            ids::CALENDAR_OVERRIDE_REL_TYPE,
        );
    serde_json::from_str(&s).expect("valid JSON")
}

fn factory(sender: &UserId) -> EventFactory {
    EventFactory::new().sender(sender)
}

fn original(
    sender: &UserId,
    id: &EventId,
    ts: u64,
    ev: CalendarEvent,
) -> Raw<AnySyncTimelineEvent> {
    factory(sender)
        .event(CalendarEventEventContent::new(ev))
        .event_id(id)
        .server_ts(ts)
        .into_raw_sync()
}

fn edit(
    sender: &UserId,
    id: &EventId,
    ts: u64,
    target: &EventId,
    ev: CalendarEvent,
) -> Raw<AnySyncTimelineEvent> {
    factory(sender)
        .event(CalendarEventEventContent::new(ev).into_replacement(target.to_owned()))
        .event_id(id)
        .server_ts(ts)
        .into_raw_sync()
}

fn override_event(
    sender: &UserId,
    id: &EventId,
    ts: u64,
    series: &EventId,
    ev: CalendarEvent,
) -> Raw<AnySyncTimelineEvent> {
    factory(sender)
        .event(CalendarEventEventContent::override_of(
            series.to_owned(),
            ev,
        ))
        .event_id(id)
        .server_ts(ts)
        .into_raw_sync()
}

fn redaction(
    sender: &UserId,
    id: &EventId,
    ts: u64,
    redacts: &EventId,
) -> Raw<AnySyncTimelineEvent> {
    factory(sender)
        .redaction(redacts)
        .event_id(id)
        .server_ts(ts)
        .into_raw_sync()
}

fn create_state(room_type: Option<&str>) -> Raw<AnySyncStateEvent> {
    let mut content = json!({ "creator": OWN, "room_version": "10" });
    if let Some(room_type) = room_type {
        content["type"] = json!(room_type);
    }
    Raw::new(&json!({
        "type": "m.room.create",
        "state_key": "",
        "event_id": "$create",
        "sender": OWN,
        "origin_server_ts": 1,
        "content": content,
    }))
    .expect("serializable")
    .cast_unchecked()
}

fn calendar_room(room_id: &RoomId) -> JoinedRoomBuilder {
    JoinedRoomBuilder::new(room_id)
        .add_state_event(create_state(Some(ids::ROOM_TYPE_CALENDAR_UNSTABLE)))
}

// ---------------------------------------------------------------------------------------------
// Calendar room API against a mocked homeserver
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn create_sends_calendar_room_type_and_info() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    server.mock_create_room().ok().mock_once().mount().await;

    let mut opts = CreateCalendarOptions::new("Personal calendar");
    opts.info = CalendarInfo::new(Some("#3c82f6".into()), Some(Berlin)).unwrap();
    opts.encrypted = true;
    opts.federate = false;
    opts.invite = vec![bob().to_owned()];

    let calendar = Calendar::create(&client, opts).await.unwrap();
    assert_eq!(calendar.room_id(), room_id!("!room:example.org"));

    let requests = server.server().received_requests().await.unwrap();
    let request = requests
        .iter()
        .find(|r| r.url.path().ends_with("/createRoom"))
        .expect("a createRoom request");
    let body: Value = request.body_json().unwrap();

    assert_eq!(
        body["creation_content"]["type"],
        ids::ROOM_TYPE_CALENDAR_UNSTABLE
    );
    assert_eq!(body["creation_content"]["m.federate"], false);
    assert_eq!(body["name"], "Personal calendar");
    assert_eq!(body["preset"], "private_chat");
    assert_eq!(body["invite"], json!([BOB]));

    let initial_state = body["initial_state"].as_array().unwrap();
    let info = initial_state
        .iter()
        .find(|e| e["type"] == ids::CALENDAR_INFO_UNSTABLE)
        .expect("calendar_info in initial_state");
    assert_eq!(info["state_key"], "");
    assert_eq!(
        info["content"],
        json!({
            "m.text": [{ "body": "Personal calendar", "mimetype": "text/plain" }],
            "org.matrix.msc4496.calendar_info": { "color": "#3c82f6", "timezone": "Europe/Berlin" }
        })
    );
    assert!(
        initial_state
            .iter()
            .any(|e| e["type"] == "m.room.encryption")
    );
}

#[tokio::test]
async fn create_federates_and_skips_encryption_by_default() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    server.mock_create_room().ok().mock_once().mount().await;

    Calendar::create(&client, CreateCalendarOptions::new("Team"))
        .await
        .unwrap();

    let requests = server.server().received_requests().await.unwrap();
    let body: Value = requests
        .iter()
        .find(|r| r.url.path().ends_with("/createRoom"))
        .unwrap()
        .body_json()
        .unwrap();
    // `m.federate: true` is the default and may be omitted on the wire.
    assert_ne!(body["creation_content"]["m.federate"], false);
    let initial_state = body["initial_state"].as_array().unwrap();
    assert_eq!(initial_state.len(), 1);
    assert_eq!(initial_state[0]["type"], ids::CALENDAR_INFO_UNSTABLE);
}

#[tokio::test]
async fn try_from_room_checks_room_type() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let unstable = server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id!("!unstable:example.org"))
                .add_state_event(create_state(Some(ids::ROOM_TYPE_CALENDAR_UNSTABLE))),
        )
        .await;
    let stable = server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id!("!stable:example.org"))
                .add_state_event(create_state(Some(ids::ROOM_TYPE_CALENDAR))),
        )
        .await;
    let plain = server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id!("!plain:example.org"))
                .add_state_event(create_state(None)),
        )
        .await;
    let space = server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id!("!space:example.org"))
                .add_state_event(create_state(Some("m.space"))),
        )
        .await;

    assert!(Calendar::try_from_room(unstable).is_ok());
    assert!(Calendar::try_from_room(stable).is_ok());
    let Err(Error::NotACalendarRoom(id)) = Calendar::try_from_room(plain) else {
        panic!("a plain room is not a calendar");
    };
    assert_eq!(id, room_id!("!plain:example.org"));
    assert!(matches!(
        Calendar::try_from_room(space),
        Err(Error::NotACalendarRoom(_))
    ));

    let mut listed: Vec<_> = list_calendars(&client)
        .iter()
        .map(|c| c.room_id().to_owned())
        .collect();
    listed.sort();
    assert_eq!(
        listed,
        vec![
            room_id!("!stable:example.org").to_owned(),
            room_id!("!unstable:example.org").to_owned()
        ]
    );
}

#[tokio::test]
async fn info_reads_stable_and_unstable_state() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let info_state = |event_type: &str| -> Raw<AnySyncStateEvent> {
        Raw::new(&json!({
            "type": event_type,
            "state_key": "",
            "event_id": "$info",
            "sender": OWN,
            "origin_server_ts": 2,
            "content": {
                "m.text": [{ "body": "Holidays", "mimetype": "text/plain" }],
                "m.calendar.info": { "color": "#ff0000", "timezone": "Europe/Berlin" }
            },
        }))
        .unwrap()
        .cast_unchecked()
    };

    let room = server
        .sync_room(
            &client,
            calendar_room(room_id!("!stable:example.org"))
                .add_state_event(info_state(ids::CALENDAR_INFO)),
        )
        .await;
    let calendar = Calendar::try_from_room(room).unwrap();
    let info = calendar.info().await.unwrap().expect("calendar_info");
    assert_eq!(info.name(), Some("Holidays"));
    assert_eq!(info.info.valid_color(), Some("#ff0000"));
    assert_eq!(info.info.timezone, Some(Berlin));

    let room = server
        .sync_room(
            &client,
            calendar_room(room_id!("!unstable:example.org"))
                .add_state_event(info_state(ids::CALENDAR_INFO_UNSTABLE)),
        )
        .await;
    let calendar = Calendar::try_from_room(room).unwrap();
    assert_eq!(
        calendar.info().await.unwrap().unwrap().name(),
        Some("Holidays")
    );

    let room = server
        .sync_room(&client, calendar_room(room_id!("!none:example.org")))
        .await;
    let calendar = Calendar::try_from_room(room).unwrap();
    assert!(calendar.info().await.unwrap().is_none());
}

#[tokio::test]
async fn create_event_sends_unstable_type() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let room = server
        .sync_room(&client, calendar_room(room_id!("!cal:example.org")))
        .await;
    let calendar = Calendar::try_from_room(room).unwrap();

    server.mock_room_state_encryption().plain().mount().await;
    server
        .mock_room_send()
        .for_type(MessageLikeEventType::from(ids::CALENDAR_EVENT_UNSTABLE))
        .ok(event_id!("$new"))
        .mock_once()
        .mount()
        .await;

    let ev = standup(user_id!("@example:localhost"));
    let event_id = calendar.create_event(ev.clone()).await.unwrap();
    assert_eq!(event_id, event_id!("$new"));

    let requests = server.server().received_requests().await.unwrap();
    let request = requests
        .iter()
        .find(|r| r.method.as_str() == "PUT" && r.url.path().contains("/send/"))
        .expect("a send request");
    assert!(
        request.url.path().starts_with(
            "/_matrix/client/v3/rooms/!cal:example.org/send/org.matrix.msc4496.calendar_event/"
        ),
        "unexpected path {}",
        request.url.path()
    );
    let body: Value = request.body_json().unwrap();
    assert_eq!(body, content_json(CalendarEventEventContent::new(ev)));
    assert_eq!(
        body["org.matrix.msc4496.calendar_event"]["uid"],
        "standup-uid"
    );
    assert_eq!(
        body["org.matrix.msc4496.calendar_event"]["title"],
        "Team standup"
    );
    assert!(body.get("m.relates_to").is_none());

    // Invalid events are rejected before anything is sent.
    let mut invalid = standup(user_id!("@example:localhost"));
    invalid.title.clear();
    assert!(calendar.create_event(invalid).await.is_err());
}

#[tokio::test]
async fn update_event_of_other_sender_is_denied() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let room_id = room_id!("!cal:example.org");

    let f = EventFactory::new().room(room_id).sender(bob());
    let bobs_event = f
        .event(CalendarEventEventContent::new(standup(bob())))
        .event_id(event_id!("$bobs"))
        .server_ts(10);

    let room = server
        .sync_room(
            &client,
            calendar_room(room_id).add_timeline_event(
                f.event(CalendarEventEventContent::new(standup(bob())))
                    .event_id(event_id!("$bobs"))
                    .server_ts(10)
                    .into_raw_sync(),
            ),
        )
        .await;
    let calendar = Calendar::try_from_room(room).unwrap();

    server
        .mock_room_event()
        .room(room_id)
        .match_event_id()
        .ok(bobs_event.into_event())
        .mount()
        .await;
    // Nothing must be sent.
    server
        .mock_room_send()
        .ok(event_id!("$never"))
        .never()
        .mount()
        .await;

    let mut changed = standup(bob());
    changed.title = "Hijacked".into();
    let result = calendar
        .update_event(event_id!("$bobs"), changed.clone())
        .await;
    assert!(
        matches!(result, Err(Error::PermissionDenied(_))),
        "{result:?}"
    );

    let result = calendar.cancel_event(event_id!("$bobs"), changed).await;
    assert!(
        matches!(result, Err(Error::PermissionDenied(_))),
        "{result:?}"
    );
}

#[tokio::test]
async fn update_and_cancel_own_event_send_replacements() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let room_id = room_id!("!cal:example.org");
    let own = user_id!("@example:localhost");
    let room = server.sync_room(&client, calendar_room(room_id)).await;
    let calendar = Calendar::try_from_room(room).unwrap();

    let mine = EventFactory::new()
        .room(room_id)
        .sender(own)
        .event(CalendarEventEventContent::new(standup(own)))
        .event_id(event_id!("$mine"))
        .server_ts(10);
    server
        .mock_room_event()
        .room(room_id)
        .match_event_id()
        .ok(mine.into_event())
        .mount()
        .await;
    server.mock_room_state_encryption().plain().mount().await;
    server
        .mock_room_send()
        .for_type(MessageLikeEventType::from(ids::CALENDAR_EVENT_UNSTABLE))
        .ok(event_id!("$edit"))
        .expect(2)
        .mount()
        .await;

    let mut moved = standup(own);
    moved.title = "Moved standup".into();
    let edit_id = calendar
        .update_event(event_id!("$mine"), moved)
        .await
        .unwrap();
    assert_eq!(edit_id, event_id!("$edit"));

    // The uid must not change.
    let mut other_uid = standup(own);
    other_uid.uid = "other".into();
    assert!(matches!(
        calendar.update_event(event_id!("$mine"), other_uid).await,
        Err(Error::InvalidIdentifier(_))
    ));

    calendar
        .cancel_event(event_id!("$mine"), standup(own))
        .await
        .unwrap();

    let requests = server.server().received_requests().await.unwrap();
    let bodies: Vec<Value> = requests
        .iter()
        .filter(|r| r.method.as_str() == "PUT" && r.url.path().contains("/send/"))
        .map(|r| r.body_json().unwrap())
        .collect();
    assert_eq!(bodies.len(), 2);
    for body in &bodies {
        assert_eq!(
            body["m.relates_to"],
            json!({ "rel_type": "m.replace", "event_id": "$mine" })
        );
    }
    assert_eq!(
        bodies[0]["m.new_content"]["org.matrix.msc4496.calendar_event"]["title"],
        "Moved standup"
    );
    assert_eq!(
        bodies[1]["m.new_content"]["org.matrix.msc4496.calendar_event"]["status"],
        "cancelled"
    );
}

#[tokio::test]
async fn create_override_sends_override_relation() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let room_id = room_id!("!cal:example.org");
    let own = user_id!("@example:localhost");
    let room = server.sync_room(&client, calendar_room(room_id)).await;
    let calendar = Calendar::try_from_room(room).unwrap();

    let series = EventFactory::new()
        .room(room_id)
        .sender(own)
        .event(CalendarEventEventContent::new(weekly_standup(own)))
        .event_id(event_id!("$series"))
        .server_ts(10);
    server
        .mock_room_event()
        .room(room_id)
        .match_event_id()
        .ok(series.into_event())
        .mount()
        .await;
    server.mock_room_state_encryption().plain().mount().await;
    server
        .mock_room_send()
        .for_type(MessageLikeEventType::from(ids::CALENDAR_EVENT_UNSTABLE))
        .ok(event_id!("$override"))
        .mock_once()
        .mount()
        .await;

    let mut instance = event(
        "standup-uid",
        "Team standup (late)",
        own,
        "2026-07-08T10:00:00",
        "2026-07-08T10:30:00",
    );
    instance.recurrence_id = Some(utc("2026-07-08T07:00:00Z"));

    // Missing recurrence_id is rejected locally.
    let mut no_rid = instance.clone();
    no_rid.recurrence_id = None;
    assert!(
        calendar
            .create_override(event_id!("$series"), no_rid)
            .await
            .is_err()
    );

    let id = calendar
        .create_override(event_id!("$series"), instance)
        .await
        .unwrap();
    assert_eq!(id, event_id!("$override"));

    let requests = server.server().received_requests().await.unwrap();
    let body: Value = requests
        .iter()
        .find(|r| r.method.as_str() == "PUT" && r.url.path().contains("/send/"))
        .unwrap()
        .body_json()
        .unwrap();
    assert_eq!(
        body["m.relates_to"],
        json!({ "rel_type": "org.matrix.msc4496.calendar_override", "event_id": "$series" })
    );
    assert_eq!(
        body["org.matrix.msc4496.calendar_event"]["recurrence_id"],
        "2026-07-08T07:00:00Z"
    );
}

#[tokio::test]
async fn create_override_on_series_made_recurring_by_edit() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let room_id = room_id!("!cal:example.org");
    let own = user_id!("@example:localhost");
    let room = server.sync_room(&client, calendar_room(room_id)).await;
    let calendar = Calendar::try_from_room(room).unwrap();

    // The original is a one-off; a later edit (not fetched) made it recurring.
    let original = EventFactory::new()
        .room(room_id)
        .sender(own)
        .event(CalendarEventEventContent::new(standup(own)))
        .event_id(event_id!("$series"))
        .server_ts(10);
    server
        .mock_room_event()
        .room(room_id)
        .match_event_id()
        .ok(original.into_event())
        .mount()
        .await;
    server.mock_room_state_encryption().plain().mount().await;
    server
        .mock_room_send()
        .for_type(MessageLikeEventType::from(ids::CALENDAR_EVENT_UNSTABLE))
        .ok(event_id!("$override"))
        .mock_once()
        .mount()
        .await;

    let mut instance = event(
        "standup-uid",
        "Team standup (late)",
        own,
        "2026-07-08T10:00:00",
        "2026-07-08T10:30:00",
    );
    instance.recurrence_id = Some(utc("2026-07-08T07:00:00Z"));
    let id = calendar
        .create_override(event_id!("$series"), instance.clone())
        .await
        .unwrap();
    assert_eq!(id, event_id!("$override"));

    // The uid check still applies.
    instance.uid = "other".into();
    assert!(matches!(
        calendar
            .create_override(event_id!("$series"), instance)
            .await,
        Err(Error::InvalidIdentifier(_))
    ));
}

#[tokio::test]
async fn update_event_of_override_keeps_recurrence_id() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let room_id = room_id!("!cal:example.org");
    let own = user_id!("@example:localhost");
    let room = server.sync_room(&client, calendar_room(room_id)).await;
    let calendar = Calendar::try_from_room(room).unwrap();

    let mut instance = event(
        "standup-uid",
        "Team standup (late)",
        own,
        "2026-07-08T10:00:00",
        "2026-07-08T10:30:00",
    );
    instance.recurrence_id = Some(utc("2026-07-08T07:00:00Z"));
    let override_event = EventFactory::new()
        .room(room_id)
        .sender(own)
        .event(CalendarEventEventContent::override_of(
            owned_event_id!("$series"),
            instance.clone(),
        ))
        .event_id(event_id!("$override"))
        .server_ts(20);
    server
        .mock_room_event()
        .room(room_id)
        .match_event_id()
        .ok(override_event.into_event())
        .mount()
        .await;
    server.mock_room_state_encryption().plain().mount().await;
    server
        .mock_room_send()
        .for_type(MessageLikeEventType::from(ids::CALENDAR_EVENT_UNSTABLE))
        .ok(event_id!("$edit"))
        .mock_once()
        .mount()
        .await;

    // Moving the override to another instance is rejected.
    let mut moved = instance.clone();
    moved.recurrence_id = Some(utc("2026-07-15T07:00:00Z"));
    assert!(matches!(
        calendar.update_event(event_id!("$override"), moved).await,
        Err(Error::InvalidIdentifier(_))
    ));

    // Editing anything else is fine.
    let mut retitled = instance;
    retitled.title = "Team standup (very late)".into();
    assert_eq!(
        calendar
            .update_event(event_id!("$override"), retitled)
            .await
            .unwrap(),
        event_id!("$edit")
    );
}

#[tokio::test]
async fn delete_event_redacts() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let room = server
        .sync_room(&client, calendar_room(room_id!("!cal:example.org")))
        .await;
    let calendar = Calendar::try_from_room(room).unwrap();

    server
        .mock_room_redact()
        .ok(event_id!("$redaction"))
        .mock_once()
        .mount()
        .await;
    calendar
        .delete_event(event_id!("$gone"), Some("spam".into()))
        .await
        .unwrap();
}

#[tokio::test]
async fn load_paginates_to_start_of_room() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let room_id = room_id!("!cal:example.org");
    let room = server.sync_room(&client, calendar_room(room_id)).await;
    let calendar = Calendar::try_from_room(room).unwrap();

    let f = EventFactory::new().room(room_id).sender(alice());
    let lunch = event(
        "lunch",
        "Lunch",
        alice(),
        "2026-07-02T12:00:00",
        "2026-07-02T13:00:00",
    );
    let mut moved_lunch = lunch.clone();
    moved_lunch.title = "Late lunch".into();

    // Backwards pagination: newest first. Page 1 holds the edit, page 2 the originals.
    let page1: Vec<Raw<AnyTimelineEvent>> = vec![
        f.event(
            CalendarEventEventContent::new(moved_lunch).into_replacement(owned_event_id!("$lunch")),
        )
        .event_id(event_id!("$lunch-edit"))
        .server_ts(30)
        .into_raw_timeline(),
        f.text_msg("unrelated")
            .event_id(event_id!("$text"))
            .server_ts(25)
            .into_raw_timeline(),
    ];
    let page2: Vec<Raw<AnyTimelineEvent>> = vec![
        f.event(CalendarEventEventContent::new(lunch))
            .event_id(event_id!("$lunch"))
            .server_ts(20)
            .into_raw_timeline(),
        f.event(CalendarEventEventContent::new(standup(alice())))
            .event_id(event_id!("$standup"))
            .server_ts(10)
            .into_raw_timeline(),
    ];

    server
        .mock_room_messages()
        .match_from("page2")
        .ok(RoomMessagesResponseTemplate::default().events(page2))
        .mock_once()
        .with_priority(1)
        .mount()
        .await;
    server
        .mock_room_messages()
        .ok(RoomMessagesResponseTemplate::default()
            .events(page1)
            .end_token("page2"))
        .mock_once()
        .with_priority(2)
        .mount()
        .await;

    let index = calendar.load().await.unwrap();
    assert_eq!(index.len(), 2);

    let lunch = index
        .get(&EventKey::new(alice().to_owned(), "lunch"))
        .unwrap();
    assert_eq!(lunch.event_id, event_id!("$lunch"));
    assert_eq!(lunch.content.title, "Late lunch");
    assert_eq!(
        lunch.last_edit_event_id.as_deref(),
        Some(event_id!("$lunch-edit"))
    );
    assert_eq!(
        index
            .by_event_id(event_id!("$standup"))
            .unwrap()
            .content
            .title,
        "Team standup"
    );

    // The requests used a filter on the calendar event types and paginated from the
    // returned end token.
    let requests = server.server().received_requests().await.unwrap();
    let messages: Vec<_> = requests
        .iter()
        .filter(|r| r.url.path().ends_with("/messages"))
        .collect();
    assert_eq!(messages.len(), 2);
    let query = |r: &wiremock::Request, key: &str| {
        r.url
            .query_pairs()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.into_owned())
    };
    assert_eq!(query(messages[0], "dir").as_deref(), Some("b"));
    assert_eq!(query(messages[0], "from"), None);
    assert_eq!(query(messages[1], "from").as_deref(), Some("page2"));
    let filter: Value = serde_json::from_str(&query(messages[0], "filter").unwrap()).unwrap();
    let types = filter["types"].as_array().unwrap();
    assert!(types.contains(&json!(ids::CALENDAR_EVENT_UNSTABLE)));
    assert!(types.contains(&json!(ids::CALENDAR_EVENT)));
}

#[tokio::test]
async fn load_continues_after_empty_chunk_with_end_token() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let room_id = room_id!("!cal:example.org");
    let room = server.sync_room(&client, calendar_room(room_id)).await;
    let calendar = Calendar::try_from_room(room).unwrap();

    let f = EventFactory::new().room(room_id).sender(alice());
    let page2: Vec<Raw<AnyTimelineEvent>> = vec![
        f.event(CalendarEventEventContent::new(standup(alice())))
            .event_id(event_id!("$standup"))
            .server_ts(10)
            .into_raw_timeline(),
    ];
    server
        .mock_room_messages()
        .match_from("page2")
        .ok(RoomMessagesResponseTemplate::default().events(page2))
        .mock_once()
        .with_priority(1)
        .mount()
        .await;
    // With the type filter, the first page has no matching events but more history.
    server
        .mock_room_messages()
        .ok(RoomMessagesResponseTemplate::default()
            .events(Vec::<Raw<AnyTimelineEvent>>::new())
            .end_token("page2"))
        .mock_once()
        .with_priority(2)
        .mount()
        .await;

    let index = calendar.load().await.unwrap();
    assert_eq!(index.len(), 1);
    assert!(index.by_event_id(event_id!("$standup")).is_some());
    let requests = server.server().received_requests().await.unwrap();
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.url.path().ends_with("/messages"))
            .count(),
        2
    );
}

#[tokio::test]
async fn load_with_max_events_stops_early() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let room_id = room_id!("!cal:example.org");
    let room = server.sync_room(&client, calendar_room(room_id)).await;
    let calendar = Calendar::try_from_room(room).unwrap();

    let f = EventFactory::new().room(room_id).sender(alice());
    let page: Vec<Raw<AnyTimelineEvent>> = vec![
        f.event(CalendarEventEventContent::new(event(
            "a",
            "A",
            alice(),
            "2026-07-02T12:00:00",
            "2026-07-02T13:00:00",
        )))
        .event_id(event_id!("$a"))
        .server_ts(20)
        .into_raw_timeline(),
    ];
    server
        .mock_room_messages()
        .match_limit(1)
        .ok(RoomMessagesResponseTemplate::default()
            .events(page)
            .end_token("more"))
        .mock_once()
        .mount()
        .await;

    let index = calendar
        .load_with(LoadOptions {
            max_events: Some(1),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(index.len(), 1);
}

// ---------------------------------------------------------------------------------------------
// CalendarIndex
// ---------------------------------------------------------------------------------------------

fn key(sender: &UserId, uid: &str) -> EventKey {
    EventKey::new(sender.to_owned(), uid)
}

#[test]
fn index_original_and_edit() {
    let mut index = CalendarIndex::new();
    assert!(index.is_empty());

    assert!(
        index
            .apply_raw_sync(&original(alice(), event_id!("$orig"), 10, standup(alice())))
            .unwrap()
    );
    let mut moved = standup(alice());
    moved.title = "Moved".into();
    assert!(
        index
            .apply_raw_sync(&edit(
                alice(),
                event_id!("$e1"),
                20,
                event_id!("$orig"),
                moved
            ))
            .unwrap()
    );
    // An older edit arriving later does not win.
    let mut older = standup(alice());
    older.title = "Older".into();
    assert!(
        !index
            .apply_raw_sync(&edit(
                alice(),
                event_id!("$e0"),
                15,
                event_id!("$orig"),
                older
            ))
            .unwrap()
    );

    assert_eq!(index.len(), 1);
    let entry = index.get(&key(alice(), "standup-uid")).unwrap();
    assert_eq!(entry.event_id, event_id!("$orig"));
    assert_eq!(entry.sender, alice());
    assert_eq!(
        entry.origin_server_ts,
        MilliSecondsSinceUnixEpoch(10u32.into())
    );
    assert_eq!(entry.content.title, "Moved");
    assert_eq!(entry.last_edit_event_id.as_deref(), Some(event_id!("$e1")));

    // Applying the same event twice is a no-op.
    assert!(
        !index
            .apply_raw_sync(&original(alice(), event_id!("$orig"), 10, standup(alice())))
            .unwrap()
    );
}

#[test]
fn index_edit_before_original() {
    let mut index = CalendarIndex::new();
    let mut moved = standup(alice());
    moved.title = "Moved".into();

    // Backwards pagination delivers the edit first.
    assert!(
        !index
            .apply_raw_sync(&edit(
                alice(),
                event_id!("$e1"),
                20,
                event_id!("$orig"),
                moved
            ))
            .unwrap()
    );
    assert!(index.is_empty());
    assert!(
        index
            .apply_raw_sync(&original(alice(), event_id!("$orig"), 10, standup(alice())))
            .unwrap()
    );

    let entry = index.by_event_id(event_id!("$orig")).unwrap();
    assert_eq!(entry.content.title, "Moved");
    assert_eq!(entry.last_edit_event_id.as_deref(), Some(event_id!("$e1")));
}

#[test]
fn index_ignores_foreign_sender_edits() {
    let mut index = CalendarIndex::new();
    index
        .apply_raw_sync(&original(alice(), event_id!("$orig"), 10, standup(alice())))
        .unwrap();

    let mut hijack = standup(alice());
    hijack.title = "Hijacked".into();
    assert!(
        !index
            .apply_raw_sync(&edit(
                bob(),
                event_id!("$bad"),
                20,
                event_id!("$orig"),
                hijack.clone()
            ))
            .unwrap()
    );
    // Also when the foreign edit arrives first.
    let mut index2 = CalendarIndex::new();
    index2
        .apply_raw_sync(&edit(
            bob(),
            event_id!("$bad"),
            20,
            event_id!("$orig"),
            hijack,
        ))
        .unwrap();
    index2
        .apply_raw_sync(&original(alice(), event_id!("$orig"), 10, standup(alice())))
        .unwrap();

    for index in [&index, &index2] {
        let entry = index.get(&key(alice(), "standup-uid")).unwrap();
        assert_eq!(entry.content.title, "Team standup");
        assert_eq!(entry.last_edit_event_id, None);
    }

    // An edit that changes the uid is ignored as well.
    let mut other_uid = standup(alice());
    other_uid.uid = "other".into();
    index
        .apply_raw_sync(&edit(
            alice(),
            event_id!("$uid"),
            30,
            event_id!("$orig"),
            other_uid,
        ))
        .unwrap();
    assert_eq!(
        index
            .get(&key(alice(), "standup-uid"))
            .unwrap()
            .last_edit_event_id,
        None
    );
}

#[test]
fn index_scopes_uid_per_sender_and_keeps_earliest_original() {
    let mut index = CalendarIndex::new();
    index
        .apply_raw_sync(&original(alice(), event_id!("$a"), 10, standup(alice())))
        .unwrap();
    // Same uid from another sender is a separate entry.
    index
        .apply_raw_sync(&original(bob(), event_id!("$b"), 20, standup(bob())))
        .unwrap();
    assert_eq!(index.len(), 2);

    // A later duplicate original from the same sender is ignored...
    let mut dup = standup(alice());
    dup.title = "Duplicate".into();
    assert!(
        !index
            .apply_raw_sync(&original(alice(), event_id!("$dup"), 30, dup))
            .unwrap()
    );
    assert_eq!(
        index.get(&key(alice(), "standup-uid")).unwrap().event_id,
        event_id!("$a")
    );

    // ...but an earlier one becomes canonical.
    let mut earlier = standup(alice());
    earlier.title = "Earlier".into();
    assert!(
        index
            .apply_raw_sync(&original(alice(), event_id!("$early"), 5, earlier))
            .unwrap()
    );
    let entry = index.get(&key(alice(), "standup-uid")).unwrap();
    assert_eq!(entry.event_id, event_id!("$early"));
    assert_eq!(entry.content.title, "Earlier");
    assert_eq!(index.len(), 2);
}

#[test]
fn index_overrides() {
    let mut index = CalendarIndex::new();
    let rid = utc("2026-07-08T07:00:00Z");
    let mut instance = event(
        "standup-uid",
        "Late standup",
        alice(),
        "2026-07-08T10:00:00",
        "2026-07-08T10:30:00",
    );
    instance.recurrence_id = Some(rid);

    // Override before its series.
    assert!(
        !index
            .apply_raw_sync(&override_event(
                alice(),
                event_id!("$o1"),
                20,
                event_id!("$series"),
                instance.clone()
            ))
            .unwrap()
    );
    assert!(
        index
            .apply_raw_sync(&original(
                alice(),
                event_id!("$series"),
                10,
                weekly_standup(alice())
            ))
            .unwrap()
    );

    let entry = index.get(&key(alice(), "standup-uid")).unwrap();
    assert_eq!(entry.overrides.len(), 1);
    assert_eq!(entry.overrides[0].event_id, event_id!("$o1"));
    assert_eq!(entry.overrides[0].recurrence_id, rid);
    assert_eq!(entry.overrides[0].content.title, "Late standup");
    assert_eq!(
        index.by_event_id(event_id!("$o1")).unwrap().event_id,
        event_id!("$series")
    );

    // Cancelling the occurrence via an edit of the override.
    let mut cancelled = instance.clone();
    cancelled.status = Status::Cancelled;
    assert!(
        index
            .apply_raw_sync(&edit(
                alice(),
                event_id!("$o1-cancel"),
                30,
                event_id!("$o1"),
                cancelled
            ))
            .unwrap()
    );
    let entry = index.get(&key(alice(), "standup-uid")).unwrap();
    assert_eq!(entry.overrides.len(), 1);
    assert!(entry.overrides[0].content.is_cancelled());
    assert_eq!(
        entry.overrides[0].last_edit_event_id.as_deref(),
        Some(event_id!("$o1-cancel"))
    );

    // A newer override for the same recurrence_id replaces it.
    let mut newer = instance.clone();
    newer.title = "Even later standup".into();
    assert!(
        index
            .apply_raw_sync(&override_event(
                alice(),
                event_id!("$o2"),
                40,
                event_id!("$series"),
                newer
            ))
            .unwrap()
    );
    let entry = index.get(&key(alice(), "standup-uid")).unwrap();
    assert_eq!(entry.overrides.len(), 1);
    assert_eq!(entry.overrides[0].event_id, event_id!("$o2"));

    // A cancelled override for another occurrence.
    let mut other = instance.clone();
    other.recurrence_id = Some(utc("2026-07-15T07:00:00Z"));
    other.status = Status::Cancelled;
    assert!(
        index
            .apply_raw_sync(&override_event(
                alice(),
                event_id!("$o3"),
                50,
                event_id!("$series"),
                other
            ))
            .unwrap()
    );
    let entry = index.get(&key(alice(), "standup-uid")).unwrap();
    assert_eq!(entry.overrides.len(), 2);
    assert!(entry.overrides[1].content.is_cancelled());

    // Overrides from another sender or with another uid are not attached.
    let mut foreign = instance.clone();
    foreign.recurrence_id = Some(utc("2026-07-22T07:00:00Z"));
    assert!(
        !index
            .apply_raw_sync(&override_event(
                bob(),
                event_id!("$bob-o"),
                60,
                event_id!("$series"),
                foreign.clone()
            ))
            .unwrap()
    );
    foreign.uid = "other".into();
    assert!(
        !index
            .apply_raw_sync(&override_event(
                alice(),
                event_id!("$uid-o"),
                60,
                event_id!("$series"),
                foreign
            ))
            .unwrap()
    );
    assert_eq!(
        index
            .get(&key(alice(), "standup-uid"))
            .unwrap()
            .overrides
            .len(),
        2
    );
    assert_eq!(index.len(), 1);

    // Redacting an override removes it.
    assert!(
        index
            .apply_raw_sync(&redaction(alice(), event_id!("$r"), 70, event_id!("$o3")))
            .unwrap()
    );
    assert_eq!(
        index
            .get(&key(alice(), "standup-uid"))
            .unwrap()
            .overrides
            .len(),
        1
    );
}

#[test]
fn index_redactions() {
    let mut index = CalendarIndex::new();
    index
        .apply_raw_sync(&original(alice(), event_id!("$orig"), 10, standup(alice())))
        .unwrap();
    let mut first = standup(alice());
    first.title = "First edit".into();
    index
        .apply_raw_sync(&edit(
            alice(),
            event_id!("$e1"),
            20,
            event_id!("$orig"),
            first,
        ))
        .unwrap();
    let mut second = standup(alice());
    second.title = "Second edit".into();
    index
        .apply_raw_sync(&edit(
            alice(),
            event_id!("$e2"),
            30,
            event_id!("$orig"),
            second,
        ))
        .unwrap();
    assert_eq!(
        index
            .get(&key(alice(), "standup-uid"))
            .unwrap()
            .content
            .title,
        "Second edit"
    );

    // Redacting the newest edit reverts to the previous one.
    assert!(
        index
            .apply_raw_sync(&redaction(alice(), event_id!("$r1"), 40, event_id!("$e2")))
            .unwrap()
    );
    let entry = index.get(&key(alice(), "standup-uid")).unwrap();
    assert_eq!(entry.content.title, "First edit");
    assert_eq!(entry.last_edit_event_id.as_deref(), Some(event_id!("$e1")));

    // Redacting the original removes the entry.
    assert!(
        index
            .apply_raw_sync(&redaction(
                alice(),
                event_id!("$r2"),
                50,
                event_id!("$orig")
            ))
            .unwrap()
    );
    assert!(index.is_empty());
    assert!(index.by_event_id(event_id!("$orig")).is_none());

    // A redaction seen before the event (backwards pagination) wins.
    let mut index = CalendarIndex::new();
    assert!(
        !index
            .apply_raw_sync(&redaction(alice(), event_id!("$r"), 50, event_id!("$late")))
            .unwrap()
    );
    assert!(
        !index
            .apply_raw_sync(&original(alice(), event_id!("$late"), 10, standup(alice())))
            .unwrap()
    );
    assert!(index.is_empty());

    // A redacted event as served by the homeserver (empty content, redacted_because).
    let mut index = CalendarIndex::new();
    index
        .apply_raw_sync(&original(alice(), event_id!("$orig"), 10, standup(alice())))
        .unwrap();
    let redacted: Raw<AnySyncTimelineEvent> = Raw::new(&json!({
        "type": ids::CALENDAR_EVENT_UNSTABLE,
        "event_id": "$orig",
        "sender": ALICE,
        "origin_server_ts": 10,
        "content": {},
        "unsigned": { "redacted_because": {
            "type": "m.room.redaction", "event_id": "$r", "sender": ALICE,
            "origin_server_ts": 60, "content": { "redacts": "$orig" }
        } }
    }))
    .unwrap()
    .cast_unchecked();
    assert!(index.apply_raw_sync(&redacted).unwrap());
    assert!(index.is_empty());
}

#[test]
fn index_accepts_stable_and_unstable_names() {
    let mut index = CalendarIndex::new();
    let rid = utc("2026-07-08T07:00:00Z");

    // Original with the stable type and block name.
    let series = to_stable(content_json(CalendarEventEventContent::new(
        weekly_standup(alice()),
    )));
    assert!(series.get(ids::CALENDAR_EVENT).is_some());
    index
        .apply_raw_sync(&raw_sync(ids::CALENDAR_EVENT, "$series", ALICE, 10, series))
        .unwrap();

    // Edit with the stable names, override with the stable rel_type.
    let mut moved = weekly_standup(alice());
    moved.title = "Stable edit".into();
    let edit = to_stable(content_json(
        CalendarEventEventContent::new(moved).into_replacement(owned_event_id!("$series")),
    ));
    index
        .apply_raw_sync(&raw_sync(ids::CALENDAR_EVENT, "$edit", ALICE, 20, edit))
        .unwrap();

    let mut instance = standup(alice());
    instance.recurrence_id = Some(rid);
    let over = to_stable(content_json(CalendarEventEventContent::override_of(
        owned_event_id!("$series"),
        instance,
    )));
    assert_eq!(
        over["m.relates_to"]["rel_type"],
        ids::CALENDAR_OVERRIDE_REL_TYPE
    );
    index
        .apply_raw_sync(&raw_sync(ids::CALENDAR_EVENT, "$over", ALICE, 30, over))
        .unwrap();

    // A plain unstable event alongside.
    let lunch = event(
        "lunch",
        "Lunch",
        alice(),
        "2026-07-02T12:00:00",
        "2026-07-02T13:00:00",
    );
    index
        .apply_raw_sync(&raw_sync(
            ids::CALENDAR_EVENT_UNSTABLE,
            "$lunch",
            ALICE,
            40,
            content_json(CalendarEventEventContent::new(lunch)),
        ))
        .unwrap();

    assert_eq!(index.len(), 2);
    let entry = index.get(&key(alice(), "standup-uid")).unwrap();
    assert_eq!(entry.content.title, "Stable edit");
    assert_eq!(entry.overrides.len(), 1);
    assert_eq!(entry.overrides[0].recurrence_id, rid);
    assert_eq!(
        index.get(&key(alice(), "lunch")).unwrap().content.title,
        "Lunch"
    );
}

#[test]
fn index_skips_invalid_and_unrelated_events() {
    let mut index = CalendarIndex::new();
    let invalid = raw_sync(
        ids::CALENDAR_EVENT_UNSTABLE,
        "$bad",
        ALICE,
        10,
        json!({ "org.matrix.msc4496.calendar_event": { "uid": "x" } }),
    );
    assert!(!index.apply_raw_sync(&invalid).unwrap());
    let message = raw_sync(
        "m.room.message",
        "$msg",
        ALICE,
        10,
        json!({ "body": "hi", "msgtype": "m.text" }),
    );
    assert!(!index.apply_raw_sync(&message).unwrap());
    assert!(index.is_empty());

    // The room-id-bearing and TimelineEvent variants work too.
    let f = EventFactory::new()
        .room(room_id!("!r:example.org"))
        .sender(alice());
    let raw: Raw<AnyTimelineEvent> = f
        .event(CalendarEventEventContent::new(standup(alice())))
        .event_id(event_id!("$a"))
        .server_ts(1)
        .into_raw_timeline();
    assert!(index.apply_raw(&raw).unwrap());
    let ev = f
        .event(CalendarEventEventContent::new(event(
            "b",
            "B",
            alice(),
            "2026-07-03T12:00:00",
            "2026-07-03T13:00:00",
        )))
        .event_id(event_id!("$b"))
        .server_ts(2)
        .into_event();
    assert!(index.apply_timeline_event(&ev).unwrap());
    assert_eq!(index.len(), 2);
}

#[test]
fn index_events_in_range() {
    let mut index = CalendarIndex::new();
    let event_ids: Vec<OwnedEventId> = (0..4)
        .map(|i| format!("$e{i}").try_into().unwrap())
        .collect();
    index
        .apply_raw_sync(&original(
            alice(),
            &event_ids[0],
            1,
            event(
                "a",
                "A",
                alice(),
                "2026-07-01T09:00:00",
                "2026-07-01T10:00:00",
            ),
        ))
        .unwrap();
    index
        .apply_raw_sync(&original(
            alice(),
            &event_ids[1],
            2,
            event(
                "b",
                "B",
                alice(),
                "2026-08-01T09:00:00",
                "2026-08-01T10:00:00",
            ),
        ))
        .unwrap();
    // A recurring series starting before the range is always included.
    let mut series = weekly_standup(alice());
    series.uid = "c".into();
    index
        .apply_raw_sync(&original(alice(), &event_ids[2], 3, series))
        .unwrap();
    // An all-day event on the last day before the range (coarse: included with slack).
    let mut all_day = CalendarEvent::new(
        "Holiday",
        EventTime::all_day("2026-06-30".parse().unwrap()),
        EventTime::all_day("2026-07-01".parse().unwrap()),
        alice().to_owned(),
    );
    all_day.uid = "d".into();
    index
        .apply_raw_sync(&original(alice(), &event_ids[3], 4, all_day))
        .unwrap();

    let start = Utc.with_ymd_and_hms(2026, 7, 1, 0, 0, 0).unwrap();
    let end = Utc.with_ymd_and_hms(2026, 7, 2, 0, 0, 0).unwrap();
    let uids: Vec<&str> = index
        .events_in_range(start, end)
        .iter()
        .map(|e| e.content.uid.as_str())
        .collect();
    assert_eq!(uids, vec!["d", "a", "c"]);
}

#[test]
fn index_organizer_defaults_to_sender() {
    let mut index = CalendarIndex::new();
    let mut implicit = standup(alice());
    implicit.organizer = None;
    index
        .apply_raw_sync(&original(alice(), event_id!("$implicit"), 10, implicit))
        .unwrap();
    let mut explicit = event(
        "other-uid",
        "Delegated",
        bob(),
        "2026-07-02T09:00:00",
        "2026-07-02T10:00:00",
    );
    explicit.organizer = Some(bob().to_owned());
    index
        .apply_raw_sync(&original(alice(), event_id!("$explicit"), 20, explicit))
        .unwrap();

    let implicit = index.get(&key(alice(), "standup-uid")).unwrap();
    assert_eq!(implicit.content.organizer, None);
    assert_eq!(implicit.organizer(), alice());
    let explicit = index.get(&key(alice(), "other-uid")).unwrap();
    assert_eq!(explicit.organizer(), bob());

    // Indexed entries serialize (and deserialize) for caching.
    let json = serde_json::to_value(implicit).unwrap();
    assert!(json["content"].get("organizer").is_none());
    let back: matrix_calendar_sdk::calendar::IndexedEvent = serde_json::from_value(json).unwrap();
    assert_eq!(&back, implicit);
}
