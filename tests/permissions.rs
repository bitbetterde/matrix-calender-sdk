//! Tests for calendar roles and the power-level scheme against a mocked homeserver.

use matrix_calendar_sdk::{
    Error,
    availability::set_policy,
    calendar::{Calendar, CreateCalendarOptions},
    events::{
        AvailabilityPolicyEventContent, CalendarEvent, CalendarInfo, CalendarInfoEventContent,
        EventTime,
    },
    ids,
    permissions::{
        CalendarMember, CalendarMembership, CalendarPermissions, CalendarRole, power_levels_for,
    },
};
use matrix_sdk::{
    Client, Room,
    ruma::{
        Int, OwnedUserId, RoomId, RoomVersionId, UserId, event_id,
        events::{AnySyncStateEvent, TimelineEventType},
        room_id,
        room_version_rules::RoomVersionRules,
        serde::Raw,
        user_id,
    },
    test_utils::mocks::MatrixMockServer,
};
use matrix_sdk_test::JoinedRoomBuilder;
use serde_json::{Value, json};
use wiremock::{
    Mock, ResponseTemplate,
    matchers::{method, path_regex},
};

/// The user the mock client is logged in as.
const OWN: &str = "@example:localhost";
const ALICE: &str = "@alice:example.org";
const BOB: &str = "@bob:example.org";
const CAROL: &str = "@carol:example.org";

fn own() -> &'static UserId {
    user_id!("@example:localhost")
}
fn alice() -> &'static UserId {
    user_id!("@alice:example.org")
}
fn bob() -> &'static UserId {
    user_id!("@bob:example.org")
}
fn carol() -> &'static UserId {
    user_id!("@carol:example.org")
}

fn state(
    event_type: &str,
    state_key: &str,
    sender: &str,
    content: Value,
) -> Raw<AnySyncStateEvent> {
    Raw::new(&json!({
        "type": event_type,
        "state_key": state_key,
        "event_id": format!("${}-{state_key}", event_type.replace('.', "-")),
        "sender": sender,
        "origin_server_ts": 1,
        "content": content,
    }))
    .expect("serializable")
    .cast_unchecked()
}

/// The create event of a calendar room created by `creator` in room version `version`.
fn create(creator: &str, version: &str) -> Raw<AnySyncStateEvent> {
    let mut content = json!({
        "room_version": version,
        "type": ids::ROOM_TYPE_CALENDAR_UNSTABLE,
    });
    if version != "12" {
        content["creator"] = json!(creator);
    }
    state("m.room.create", "", creator, content)
}

/// The SDK scheme (default permissions) with the given `users` map, as JSON.
fn scheme(users: Value, rules: &RoomVersionRules) -> Value {
    let mut content = serde_json::to_value(
        power_levels_for(&CalendarPermissions::default(), &rules.authorization).unwrap(),
    )
    .unwrap();
    content["users"] = users;
    content
}

/// The power levels Synapse creates for a `private_chat` room before version 12.
fn matrix_defaults(creator: &str) -> Value {
    json!({
        "ban": 50,
        "events": {
            "m.room.avatar": 50,
            "m.room.canonical_alias": 50,
            "m.room.encryption": 100,
            "m.room.history_visibility": 100,
            "m.room.name": 50,
            "m.room.power_levels": 100,
            "m.room.server_acl": 100,
            "m.room.tombstone": 100
        },
        "events_default": 0,
        "historical": 100,
        "invite": 0,
        "kick": 50,
        "redact": 50,
        "state_default": 50,
        "users": { creator: 100 },
        "users_default": 0
    })
}

/// Syncs a calendar room with the given create event and power levels.
async fn room_with(
    server: &MatrixMockServer,
    client: &Client,
    room_id: &RoomId,
    create_event: Raw<AnySyncStateEvent>,
    power_levels: &Value,
) -> Room {
    server
        .sync_room(
            client,
            JoinedRoomBuilder::new(room_id)
                .add_state_event(create_event)
                .add_state_event(state("m.room.power_levels", "", OWN, power_levels.clone())),
        )
        .await
}

/// Serves `content` for `GET .../state/m.room.power_levels/`.
async fn mock_power_levels_on_server(server: &MatrixMockServer, content: &Value) {
    Mock::given(method("GET"))
        .and(path_regex(r"/state/m\.room\.power_levels/?$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(content.clone()))
        .mount(server.server())
        .await;
}

/// Serves the membership of `user` (or 404 for `None`) for `GET .../state/m.room.member/user`.
async fn mock_membership_on_server(
    server: &MatrixMockServer,
    user: &str,
    membership: Option<&str>,
) {
    let response = match membership {
        Some(membership) => {
            ResponseTemplate::new(200).set_body_json(json!({ "membership": membership }))
        }
        None => ResponseTemplate::new(404)
            .set_body_json(json!({ "errcode": "M_NOT_FOUND", "error": "Event not found." })),
    };
    Mock::given(method("GET"))
        .and(path_regex(format!(
            r"/state/m\.room\.member/{}$",
            regex_escape(user)
        )))
        .respond_with(response)
        .mount(server.server())
        .await;
}

fn regex_escape(s: &str) -> String {
    s.replace('.', r"\.")
}

/// `(method, path, JSON body)` of every room-related request the server received, in order.
async fn received(server: &MatrixMockServer) -> Vec<(String, String, Option<Value>)> {
    server
        .server()
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        // Only room requests and room creation, not the client's own key uploads.
        .filter(|r| r.url.path().contains("/rooms/") || r.url.path().ends_with("/createRoom"))
        .map(|r| {
            (
                r.method.to_string(),
                r.url.path().to_owned(),
                r.body_json::<Value>().ok(),
            )
        })
        .collect()
}

fn sent_power_levels(requests: &[(String, String, Option<Value>)]) -> Vec<Value> {
    requests
        .iter()
        .filter(|(m, p, _)| m == "PUT" && p.contains("/state/m.room.power_levels"))
        .map(|(_, _, body)| body.clone().expect("JSON body"))
        .collect()
}

fn event_for(organizer: &UserId) -> CalendarEvent {
    let start = "2026-07-01T09:00:00".parse().unwrap();
    let end = "2026-07-01T10:00:00".parse().unwrap();
    CalendarEvent::new(
        "Planning",
        EventTime::timed(start, chrono_tz::Europe::Berlin).unwrap(),
        EventTime::timed(end, chrono_tz::Europe::Berlin).unwrap(),
        organizer.to_owned(),
    )
}

// ---------------------------------------------------------------------------------------------
// The scheme
// ---------------------------------------------------------------------------------------------

#[test]
fn power_levels_for_every_option_combination() {
    let v11 = RoomVersionRules::V11.authorization;
    for default_role in [CalendarRole::Reader, CalendarRole::Editor] {
        for readers_can_rsvp in [false, true] {
            for editors_can_invite in [false, true] {
                let mut perms = CalendarPermissions::new(default_role);
                perms.readers_can_rsvp = readers_can_rsvp;
                perms.editors_can_invite = editors_can_invite;
                let content = power_levels_for(&perms, &v11).unwrap();
                let what = format!("{perms:?}");

                assert_eq!(
                    i64::from(content.users_default),
                    default_role.power_level(),
                    "{what}"
                );
                assert_eq!(i64::from(content.events_default), 50, "{what}");
                assert_eq!(i64::from(content.state_default), 100, "{what}");
                assert_eq!(i64::from(content.ban), 100, "{what}");
                assert_eq!(i64::from(content.kick), 100, "{what}");
                assert_eq!(i64::from(content.redact), 100, "{what}");
                assert_eq!(
                    i64::from(content.invite),
                    if editors_can_invite { 50 } else { 100 },
                    "{what}"
                );
                assert!(content.users.is_empty(), "{what}");

                let level = |t: &str| {
                    content
                        .events
                        .get(&TimelineEventType::from(t))
                        .map(|l| i64::from(*l))
                        .unwrap_or_else(|| panic!("{what}: no level for {t}"))
                };
                let rsvp = if readers_can_rsvp { 0 } else { 50 };
                for (t, expected) in [
                    (ids::CALENDAR_EVENT, 50),
                    (ids::CALENDAR_EVENT_UNSTABLE, 50),
                    (ids::CALENDAR_INVITE, 50),
                    (ids::CALENDAR_INVITE_UNSTABLE, 50),
                    (ids::CALENDAR_RSVP, rsvp),
                    (ids::CALENDAR_RSVP_UNSTABLE, rsvp),
                    ("m.reaction", 0),
                    ("m.room.redaction", 0),
                    (ids::CALENDAR_INFO, 100),
                    (ids::CALENDAR_INFO_UNSTABLE, 100),
                    (ids::AVAILABILITY_POLICY, 100),
                    (ids::AVAILABILITY_POLICY_UNSTABLE, 100),
                    (ids::AVAILABILITY_WINDOW, 100),
                    (ids::AVAILABILITY_WINDOW_UNSTABLE, 100),
                    ("m.room.name", 100),
                    ("m.room.topic", 100),
                    ("m.room.avatar", 100),
                    ("m.room.power_levels", 100),
                    ("m.room.history_visibility", 100),
                    ("m.room.canonical_alias", 100),
                    ("m.room.encryption", 100),
                    ("m.room.server_acl", 100),
                    ("m.room.tombstone", 100),
                ] {
                    assert_eq!(level(t), expected, "{what}: {t}");
                }
            }
        }
    }
}

#[test]
fn power_levels_for_rejects_owner_default_and_keeps_v12_tombstone() {
    let v12 = RoomVersionRules::V12.authorization;
    assert!(matches!(
        power_levels_for(&CalendarPermissions::new(CalendarRole::Owner), &v12),
        Err(Error::InvalidPermissions(_))
    ));
    let content = power_levels_for(&CalendarPermissions::default(), &v12).unwrap();
    // Matrix's room version 12 default: only creators can upgrade the room.
    assert_eq!(
        content.events[&TimelineEventType::RoomTombstone],
        Int::from(150u8)
    );
}

#[test]
fn role_helpers() {
    assert_eq!(CalendarRole::Reader.power_level(), 0);
    assert_eq!(CalendarRole::Editor.power_level(), 50);
    assert_eq!(CalendarRole::Owner.power_level(), 100);
    assert!(!CalendarRole::Reader.can_write());
    assert!(CalendarRole::Editor.can_write() && !CalendarRole::Editor.can_manage());
    assert!(CalendarRole::Owner.can_write() && CalendarRole::Owner.can_manage());
    assert!(
        CalendarRole::Reader < CalendarRole::Editor && CalendarRole::Editor < CalendarRole::Owner
    );
    assert_eq!(
        serde_json::to_value(CalendarRole::Editor).unwrap(),
        json!("editor")
    );
    assert_eq!(
        serde_json::from_value::<CalendarRole>(json!("reader")).unwrap(),
        CalendarRole::Reader
    );
    assert_eq!(CalendarRole::Owner.to_string(), "owner");
    let defaults = CalendarPermissions::default();
    assert_eq!(defaults.default_role, CalendarRole::Reader);
    assert!(defaults.readers_can_rsvp);
    assert!(!defaults.editors_can_invite);
}

// ---------------------------------------------------------------------------------------------
// Calendar creation
// ---------------------------------------------------------------------------------------------

async fn create_body(server: &MatrixMockServer) -> Value {
    received(server)
        .await
        .into_iter()
        .find(|(_, p, _)| p.ends_with("/createRoom"))
        .and_then(|(_, _, body)| body)
        .expect("a createRoom request")
}

#[tokio::test]
async fn create_sends_power_level_override_with_member_roles() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    server.mock_create_room().ok().mock_once().mount().await;

    let mut opts = CreateCalendarOptions::new("Team");
    opts.room_version = Some(RoomVersionId::V12);
    opts.members = vec![
        (bob().to_owned(), CalendarRole::Editor),
        (carol().to_owned(), CalendarRole::Reader),
        // The creator is never put into the users map in room version 12.
        (own().to_owned(), CalendarRole::Reader),
    ];
    opts.invite = vec![alice().to_owned(), bob().to_owned()];
    Calendar::create(&client, opts).await.unwrap();

    let body = create_body(&server).await;
    assert_eq!(body["room_version"], "12");
    assert_eq!(body["invite"], json!([ALICE, BOB, CAROL]));
    let pl = &body["power_level_content_override"];
    assert_eq!(pl["users"], json!({ BOB: 50, CAROL: 0 }));
    assert_eq!(pl["users_default"], 0);
    assert_eq!(pl["events_default"], 50);
    assert_eq!(pl["state_default"], 100);
    assert_eq!(pl["invite"], 100);
    assert_eq!(pl["kick"], 100);
    assert_eq!(pl["ban"], 100);
    assert_eq!(pl["redact"], 100);
    let events = &pl["events"];
    for t in [
        ids::CALENDAR_EVENT,
        ids::CALENDAR_EVENT_UNSTABLE,
        ids::CALENDAR_INVITE,
        ids::CALENDAR_INVITE_UNSTABLE,
    ] {
        assert_eq!(events[t], 50, "{t}");
    }
    for t in [
        ids::CALENDAR_RSVP,
        ids::CALENDAR_RSVP_UNSTABLE,
        "m.reaction",
        "m.room.redaction",
    ] {
        assert_eq!(events[t], 0, "{t}");
    }
    for t in [
        ids::CALENDAR_INFO,
        ids::CALENDAR_INFO_UNSTABLE,
        ids::AVAILABILITY_POLICY,
        ids::AVAILABILITY_POLICY_UNSTABLE,
        ids::AVAILABILITY_WINDOW,
        ids::AVAILABILITY_WINDOW_UNSTABLE,
        "m.room.power_levels",
        "m.room.name",
    ] {
        assert_eq!(events[t], 100, "{t}");
    }
    assert_eq!(events["m.room.tombstone"], 150);
}

#[tokio::test]
async fn create_uses_server_default_room_version_for_the_creator() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    server.mock_create_room().ok().mock_once().mount().await;
    Mock::given(method("GET"))
        .and(path_regex(r"/capabilities$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "capabilities": {
                "m.room_versions": { "default": "10", "available": { "10": "stable", "12": "stable" } }
            }
        })))
        .mount(server.server())
        .await;

    let mut opts = CreateCalendarOptions::new("Team");
    opts.members = vec![(bob().to_owned(), CalendarRole::Editor)];
    let mut perms = CalendarPermissions::new(CalendarRole::Editor);
    perms.readers_can_rsvp = false;
    perms.editors_can_invite = true;
    opts.permissions = perms;
    Calendar::create(&client, opts).await.unwrap();

    let body = create_body(&server).await;
    assert_eq!(body["room_version"], "10");
    let pl = &body["power_level_content_override"];
    // Before room version 12 the creator must stay in the users map.
    assert_eq!(pl["users"], json!({ OWN: 100, BOB: 50 }));
    assert_eq!(pl["users_default"], 50);
    assert_eq!(pl["invite"], 50);
    assert_eq!(pl["events"][ids::CALENDAR_RSVP_UNSTABLE], 50);
    assert_eq!(pl["events"]["m.room.tombstone"], 100);
}

#[tokio::test]
async fn create_without_explicit_roles_keeps_server_default_users() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    server.mock_create_room().ok().mock_once().mount().await;

    let mut opts = CreateCalendarOptions::new("Team");
    opts.invite = vec![bob().to_owned()];
    opts.room_version = Some(RoomVersionId::V11);
    Calendar::create(&client, opts).await.unwrap();

    let body = create_body(&server).await;
    assert_eq!(body["invite"], json!([BOB]));
    let pl = &body["power_level_content_override"];
    assert!(pl.get("users").is_none(), "{pl}");
    assert_eq!(pl["users_default"], 0);
    assert_eq!(pl["events_default"], 50);

    // The owner role as default is refused before anything is sent.
    let mut opts = CreateCalendarOptions::new("Bad");
    opts.permissions = CalendarPermissions::new(CalendarRole::Owner);
    assert!(matches!(
        Calendar::create(&client, opts).await,
        Err(Error::InvalidPermissions(_))
    ));
}

// ---------------------------------------------------------------------------------------------
// Inviting and roles
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn invite_sets_power_level_before_inviting() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let pl = scheme(json!({ OWN: 100 }), &RoomVersionRules::V10);
    let room = room_with(
        &server,
        &client,
        room_id!("!cal:example.org"),
        create(OWN, "10"),
        &pl,
    )
    .await;
    let calendar = Calendar::try_from_room(room).unwrap();

    mock_power_levels_on_server(&server, &pl).await;
    mock_membership_on_server(&server, BOB, None).await;
    server
        .mock_room_send_state()
        .ok(event_id!("$pl"))
        .mock_once()
        .mount()
        .await;
    server
        .mock_invite_user_by_id()
        .ok()
        .mock_once()
        .mount()
        .await;

    calendar.invite_reader(bob()).await.unwrap();

    let requests = received(&server).await;
    let set_index = requests
        .iter()
        .position(|(m, p, _)| m == "PUT" && p.contains("/state/m.room.power_levels"))
        .expect("power levels sent");
    let invite = requests
        .iter()
        .position(|(m, p, _)| m == "POST" && p.ends_with("/invite"))
        .expect("invite sent");
    assert!(
        set_index < invite,
        "the role must be set before the invite: {requests:?}"
    );
    let sent = &sent_power_levels(&requests)[0];
    // The reader role is pinned explicitly, and nothing else changes.
    let mut expected = pl.clone();
    expected["users"][BOB] = json!(0);
    assert_eq!(sent, &expected);
    assert_eq!(requests[invite].2.as_ref().unwrap()["user_id"], BOB);
}

#[tokio::test]
async fn invite_of_joined_member_only_adjusts_role() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let pl = scheme(json!({ OWN: 100, BOB: 0 }), &RoomVersionRules::V10);
    let room = room_with(
        &server,
        &client,
        room_id!("!cal:example.org"),
        create(OWN, "10"),
        &pl,
    )
    .await;
    let calendar = Calendar::try_from_room(room).unwrap();

    mock_power_levels_on_server(&server, &pl).await;
    mock_membership_on_server(&server, BOB, Some("join")).await;
    server
        .mock_room_send_state()
        .ok(event_id!("$pl"))
        .mock_once()
        .mount()
        .await;
    server.mock_invite_user_by_id().ok().never().mount().await;

    calendar.invite_editor(bob()).await.unwrap();
    let requests = received(&server).await;
    assert_eq!(sent_power_levels(&requests)[0]["users"][BOB], 50);
    assert!(!requests.iter().any(|(_, p, _)| p.ends_with("/invite")));

    // Asking again for the role bob already has sends nothing.
    server.server().reset().await;
    let pl = scheme(json!({ OWN: 100, BOB: 50 }), &RoomVersionRules::V10);
    mock_power_levels_on_server(&server, &pl).await;
    mock_membership_on_server(&server, BOB, Some("join")).await;
    calendar.invite_editor(bob()).await.unwrap();
    let requests = received(&server).await;
    assert!(requests.iter().all(|(m, _, _)| m == "GET"), "{requests:?}");
}

#[tokio::test]
async fn invite_as_editor_is_denied_up_front() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let pl = scheme(json!({ ALICE: 100, OWN: 50 }), &RoomVersionRules::V10);
    let room = room_with(
        &server,
        &client,
        room_id!("!cal:example.org"),
        create(ALICE, "10"),
        &pl,
    )
    .await;
    let calendar = Calendar::try_from_room(room).unwrap();
    mock_power_levels_on_server(&server, &pl).await;
    mock_membership_on_server(&server, BOB, None).await;

    let Err(Error::PermissionDenied(message)) = calendar.invite_reader(bob()).await else {
        panic!("an editor cannot invite by default");
    };
    assert!(message.contains("invite"), "{message}");
    let requests = received(&server).await;
    assert!(requests.iter().all(|(m, _, _)| m == "GET"), "{requests:?}");
}

#[tokio::test]
async fn role_of_sdk_scheme_room() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let pl = scheme(
        json!({ OWN: 100, ALICE: 100, BOB: 50, CAROL: 0 }),
        &RoomVersionRules::V10,
    );
    let room = room_with(
        &server,
        &client,
        room_id!("!cal:example.org"),
        create(OWN, "10"),
        &pl,
    )
    .await;
    let calendar = Calendar::try_from_room(room).unwrap();

    assert_eq!(calendar.my_role().await.unwrap(), CalendarRole::Owner);
    assert_eq!(
        calendar.role_of(alice()).await.unwrap(),
        CalendarRole::Owner
    );
    assert_eq!(calendar.role_of(bob()).await.unwrap(), CalendarRole::Editor);
    assert_eq!(
        calendar.role_of(carol()).await.unwrap(),
        CalendarRole::Reader
    );
    assert_eq!(
        calendar
            .role_of(user_id!("@stranger:example.org"))
            .await
            .unwrap(),
        CalendarRole::Reader
    );
    assert!(calendar.can_write().await.unwrap());

    let summary = calendar.permissions().await.unwrap();
    assert_eq!(summary.default_role, CalendarRole::Reader);
    assert!(summary.readers_can_rsvp);
    assert!(!summary.editors_can_invite);
    assert!(summary.is_standard);
    assert_eq!(summary.as_permissions(), CalendarPermissions::default());
}

#[tokio::test]
async fn role_of_room_with_matrix_default_power_levels() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let room = room_with(
        &server,
        &client,
        room_id!("!legacy:example.org"),
        create(ALICE, "10"),
        &matrix_defaults(ALICE),
    )
    .await;
    let calendar = Calendar::try_from_room(room).unwrap();

    // Everybody can send calendar events: the creator is an owner, everyone else an editor.
    assert_eq!(
        calendar.role_of(alice()).await.unwrap(),
        CalendarRole::Owner
    );
    assert_eq!(calendar.my_role().await.unwrap(), CalendarRole::Editor);
    assert_eq!(calendar.role_of(bob()).await.unwrap(), CalendarRole::Editor);
    let summary = calendar.permissions().await.unwrap();
    assert_eq!(summary.default_role, CalendarRole::Editor);
    assert!(!summary.is_standard);
}

#[tokio::test]
async fn role_of_v12_creator_absent_from_users() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let pl = scheme(json!({ BOB: 50 }), &RoomVersionRules::V12);
    let room = room_with(
        &server,
        &client,
        room_id!("!v12:example.org"),
        create(OWN, "12"),
        &pl,
    )
    .await;
    let calendar = Calendar::try_from_room(room).unwrap();

    assert_eq!(calendar.my_role().await.unwrap(), CalendarRole::Owner);
    assert_eq!(calendar.role_of(bob()).await.unwrap(), CalendarRole::Editor);
    assert_eq!(
        calendar.role_of(carol()).await.unwrap(),
        CalendarRole::Reader
    );
    assert!(calendar.permissions().await.unwrap().is_standard);

    // The creator cannot be demoted; asking for the owner role is a no-op.
    mock_power_levels_on_server(&server, &pl).await;
    let Err(Error::PermissionDenied(message)) =
        calendar.set_role(own(), CalendarRole::Editor).await
    else {
        panic!("a v12 creator cannot be demoted");
    };
    assert!(message.contains("creator"), "{message}");
    calendar.set_role(own(), CalendarRole::Owner).await.unwrap();
    assert!(sent_power_levels(&received(&server).await).is_empty());

    // Changing someone else's role never puts the creator into `users`.
    server
        .mock_room_send_state()
        .ok(event_id!("$pl"))
        .mock_once()
        .mount()
        .await;
    calendar
        .set_role(carol(), CalendarRole::Editor)
        .await
        .unwrap();
    let sent = sent_power_levels(&received(&server).await);
    assert_eq!(sent[0]["users"], json!({ BOB: 50, CAROL: 50 }));
}

#[tokio::test]
async fn reader_cannot_write_and_nothing_is_sent() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let pl = scheme(json!({ ALICE: 100 }), &RoomVersionRules::V10);
    let room = room_with(
        &server,
        &client,
        room_id!("!cal:example.org"),
        create(ALICE, "10"),
        &pl,
    )
    .await;
    let calendar = Calendar::try_from_room(room.clone()).unwrap();

    assert_eq!(calendar.my_role().await.unwrap(), CalendarRole::Reader);
    assert!(!calendar.can_write().await.unwrap());

    let Err(Error::PermissionDenied(message)) = calendar.create_event(event_for(own())).await
    else {
        panic!("a reader cannot create events");
    };
    assert!(message.contains(ids::CALENDAR_EVENT_UNSTABLE), "{message}");
    assert!(message.contains("reader"), "{message}");

    let edit = calendar
        .update_event(matrix_sdk::ruma::event_id!("$x"), event_for(own()))
        .await;
    assert!(matches!(edit, Err(Error::PermissionDenied(_))), "{edit:?}");
    let cancel = calendar
        .cancel_event(matrix_sdk::ruma::event_id!("$x"), event_for(own()))
        .await;
    assert!(
        matches!(cancel, Err(Error::PermissionDenied(_))),
        "{cancel:?}"
    );

    let info = CalendarInfoEventContent::new("Cal", CalendarInfo::default()).unwrap();
    let Err(Error::PermissionDenied(message)) = calendar.set_info(info).await else {
        panic!("a reader cannot change the calendar info");
    };
    assert!(message.contains(ids::CALENDAR_INFO_UNSTABLE), "{message}");
    assert!(matches!(
        set_policy(&room, AvailabilityPolicyEventContent::default()).await,
        Err(Error::PermissionDenied(_))
    ));

    // Nothing reached the server.
    assert!(received(&server).await.is_empty());
}

#[tokio::test]
async fn editor_cannot_change_info() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let pl = scheme(json!({ ALICE: 100, OWN: 50 }), &RoomVersionRules::V10);
    let room = room_with(
        &server,
        &client,
        room_id!("!cal:example.org"),
        create(ALICE, "10"),
        &pl,
    )
    .await;
    let calendar = Calendar::try_from_room(room).unwrap();
    assert_eq!(calendar.my_role().await.unwrap(), CalendarRole::Editor);
    let info = CalendarInfoEventContent::new("Cal", CalendarInfo::default()).unwrap();
    assert!(matches!(
        calendar.set_info(info).await,
        Err(Error::PermissionDenied(_))
    ));
    assert!(received(&server).await.is_empty());
}

#[tokio::test]
async fn last_owner_cannot_demote_themselves() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let pl = scheme(json!({ OWN: 100, BOB: 50 }), &RoomVersionRules::V10);
    let room = room_with(
        &server,
        &client,
        room_id!("!cal:example.org"),
        create(OWN, "10"),
        &pl,
    )
    .await;
    let calendar = Calendar::try_from_room(room).unwrap();
    mock_power_levels_on_server(&server, &pl).await;

    let Err(Error::PermissionDenied(message)) =
        calendar.set_role(own(), CalendarRole::Editor).await
    else {
        panic!("the last owner cannot demote themselves");
    };
    assert!(message.contains("last owner"), "{message}");
    assert!(sent_power_levels(&received(&server).await).is_empty());

    // An owner who has only been invited does not count; a joined one does.
    server.server().reset().await;
    let pl = scheme(
        json!({ OWN: 100, ALICE: 100, CAROL: 100 }),
        &RoomVersionRules::V10,
    );
    mock_power_levels_on_server(&server, &pl).await;
    mock_membership_on_server(&server, ALICE, Some("invite")).await;
    mock_membership_on_server(&server, CAROL, None).await;
    assert!(matches!(
        calendar.set_role(own(), CalendarRole::Reader).await,
        Err(Error::PermissionDenied(_))
    ));
    server.server().reset().await;
    mock_power_levels_on_server(&server, &pl).await;
    mock_membership_on_server(&server, ALICE, Some("join")).await;
    mock_membership_on_server(&server, CAROL, None).await;
    server
        .mock_room_send_state()
        .ok(event_id!("$pl"))
        .mock_once()
        .mount()
        .await;
    calendar
        .set_role(own(), CalendarRole::Reader)
        .await
        .unwrap();
    assert_eq!(
        sent_power_levels(&received(&server).await)[0]["users"][OWN],
        0
    );
}

#[tokio::test]
async fn set_role_in_legacy_room_needs_the_scheme() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let pl = matrix_defaults(OWN);
    let room = room_with(
        &server,
        &client,
        room_id!("!legacy:example.org"),
        create(OWN, "10"),
        &pl,
    )
    .await;
    let calendar = Calendar::try_from_room(room).unwrap();
    mock_power_levels_on_server(&server, &pl).await;

    // Level 0 can still send calendar events here, so it would not make bob a reader.
    assert!(matches!(
        calendar.set_role(bob(), CalendarRole::Reader).await,
        Err(Error::InvalidPermissions(_))
    ));

    // apply_permissions rewrites the room-wide levels and keeps users and unknown keys.
    server
        .mock_room_send_state()
        .ok(event_id!("$pl"))
        .mock_once()
        .mount()
        .await;
    calendar
        .apply_permissions(&CalendarPermissions::default())
        .await
        .unwrap();
    let sent = sent_power_levels(&received(&server).await).remove(0);
    let mut expected = scheme(json!({ OWN: 100 }), &RoomVersionRules::V10);
    expected["historical"] = json!(100);
    // Every level of the scheme is spelled out, including those equal to Matrix's defaults.
    expected["users_default"] = json!(0);
    assert_eq!(sent, expected);
}

#[tokio::test]
async fn apply_permissions_normalizing_users() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let mut pl = matrix_defaults(OWN);
    pl["users"] = json!({ OWN: 100, ALICE: 75, BOB: 30, CAROL: 10 });
    let room = room_with(
        &server,
        &client,
        room_id!("!legacy:example.org"),
        create(OWN, "10"),
        &pl,
    )
    .await;
    let calendar = Calendar::try_from_room(room).unwrap();
    mock_power_levels_on_server(&server, &pl).await;
    server
        .mock_room_send_state()
        .ok(event_id!("$pl"))
        .mock_once()
        .mount()
        .await;

    let mut perms = CalendarPermissions::new(CalendarRole::Editor);
    perms.readers_can_rsvp = false;
    calendar
        .apply_permissions_normalizing_users(&perms)
        .await
        .unwrap();
    let sent = sent_power_levels(&received(&server).await).remove(0);
    assert_eq!(
        sent["users"],
        json!({ OWN: 100, ALICE: 50, BOB: 50, CAROL: 0 })
    );
    assert_eq!(sent["users_default"], 50);
    assert_eq!(sent["events"][ids::CALENDAR_RSVP], 50);
}

#[tokio::test]
async fn members_with_roles_sorted_by_role() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let pl = scheme(json!({ OWN: 100, BOB: 50 }), &RoomVersionRules::V10);
    let room = room_with(
        &server,
        &client,
        room_id!("!cal:example.org"),
        create(OWN, "10"),
        &pl,
    )
    .await;
    let calendar = Calendar::try_from_room(room).unwrap();

    let member = |user: &str, membership: &str, name: Option<&str>| {
        let mut content = json!({ "membership": membership });
        if let Some(name) = name {
            content["displayname"] = json!(name);
        }
        Raw::new(&json!({
            "type": "m.room.member",
            "state_key": user,
            "event_id": format!("$member-{user}"),
            "sender": user,
            "origin_server_ts": 1,
            "room_id": "!cal:example.org",
            "content": content,
        }))
        .unwrap()
        .cast_unchecked()
    };
    server
        .mock_get_members()
        .ok(vec![
            member(CAROL, "invite", None),
            member(BOB, "join", Some("Bob")),
            member(OWN, "join", None),
            member(ALICE, "leave", None),
        ])
        .mount()
        .await;

    let members = calendar.members_with_roles().await.unwrap();
    let summary: Vec<(OwnedUserId, CalendarMembership, CalendarRole)> = members
        .iter()
        .map(|m: &CalendarMember| (m.user_id.clone(), m.membership, m.role))
        .collect();
    assert_eq!(
        summary,
        vec![
            (
                own().to_owned(),
                CalendarMembership::Joined,
                CalendarRole::Owner
            ),
            (
                bob().to_owned(),
                CalendarMembership::Joined,
                CalendarRole::Editor
            ),
            (
                carol().to_owned(),
                CalendarMembership::Invited,
                CalendarRole::Reader
            ),
        ]
    );
    assert_eq!(members[1].display_name.as_deref(), Some("Bob"));
}
