//! Tests for pending invitations to calendar rooms (sharing a whole calendar by room
//! membership), against a mocked homeserver.
//!
//! These cover what the SDK can decide locally from the **stripped** invite state, in
//! particular the role fallback when the homeserver leaves `m.room.power_levels` out of it
//! (which Synapse does; see `tests/e2e.rs` for the live counterpart).

use matrix_calendar_sdk::{
    Error,
    calendar::{Calendar, CalendarInvitation, list_calendar_invitations, list_calendars},
    events::{CalendarInfo, CalendarInfoEventContent},
    ids,
    permissions::CalendarRole,
};
use matrix_sdk::{
    Client, RoomState,
    ruma::{
        RoomId, UserId,
        events::{AnyStrippedStateEvent, AnySyncStateEvent},
        room_id,
        serde::Raw,
        user_id,
    },
    test_utils::mocks::MatrixMockServer,
};
use matrix_sdk_test::{InvitedRoomBuilder, JoinedRoomBuilder};
use serde_json::{Value, json};

const OWN: &str = "@example:localhost";
const ALICE: &str = "@alice:example.org";

fn own() -> &'static UserId {
    user_id!("@example:localhost")
}

fn alice() -> &'static UserId {
    user_id!("@alice:example.org")
}

/// A stripped state event (no `event_id` or `origin_server_ts`, as in `invite_state`).
fn stripped(
    event_type: &str,
    state_key: &str,
    sender: &str,
    content: Value,
) -> Raw<AnyStrippedStateEvent> {
    Raw::new(&json!({
        "type": event_type,
        "state_key": state_key,
        "sender": sender,
        "content": content,
    }))
    .expect("serializable")
    .cast_unchecked()
}

fn create(room_type: Option<&str>) -> Value {
    let mut content = json!({ "room_version": "11", "creator": ALICE });
    if let Some(room_type) = room_type {
        content["type"] = json!(room_type);
    }
    content
}

/// The stripped state Synapse actually sends for an invite to a calendar room: the create
/// event (with the room type), the join rules, the room name and the member events — but
/// neither the `calendar_info` nor the `m.room.power_levels` state event.
fn synapse_invite_state(name: &str) -> Vec<Raw<AnyStrippedStateEvent>> {
    vec![
        stripped(
            "m.room.create",
            "",
            ALICE,
            create(Some(ids::ROOM_TYPE_CALENDAR_UNSTABLE)),
        ),
        stripped(
            "m.room.join_rules",
            "",
            ALICE,
            json!({ "join_rule": "invite" }),
        ),
        stripped("m.room.name", "", ALICE, json!({ "name": name })),
        stripped(
            "m.room.member",
            ALICE,
            ALICE,
            json!({ "membership": "join", "displayname": "alice" }),
        ),
        stripped(
            "m.room.member",
            OWN,
            ALICE,
            json!({ "membership": "invite", "displayname": "example" }),
        ),
    ]
}

/// Syncs an invite to `room_id` with the given stripped state and returns the invitation.
async fn invitation(
    server: &MatrixMockServer,
    client: &Client,
    room_id: &RoomId,
    state: Vec<Raw<AnyStrippedStateEvent>>,
) -> CalendarInvitation {
    let room = server
        .sync_room(
            client,
            InvitedRoomBuilder::new(room_id).add_state_bulk(state),
        )
        .await;
    assert_eq!(room.state(), RoomState::Invited);
    CalendarInvitation::try_from_room(room).expect("a calendar invitation")
}

fn calendar_info_content(name: &str, color: Option<&str>) -> Value {
    serde_json::to_value(
        CalendarInfoEventContent::new(
            name,
            CalendarInfo::new(
                color.map(ToOwned::to_owned),
                Some(chrono_tz::Europe::Berlin),
            )
            .expect("valid info"),
        )
        .expect("valid content"),
    )
    .expect("serializable")
}

// ---------------------------------------------------------------------------------------------
// Listing and filtering
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn lists_only_invites_to_calendar_rooms() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let unstable = room_id!("!unstable:example.org");
    let stable = room_id!("!stable:example.org");
    let chat = room_id!("!chat:example.org");
    let untyped = room_id!("!untyped:example.org");

    server
        .sync_room(
            &client,
            InvitedRoomBuilder::new(unstable).add_state_bulk(synapse_invite_state("Unstable")),
        )
        .await;
    server
        .sync_room(
            &client,
            InvitedRoomBuilder::new(stable).add_state_event(stripped(
                "m.room.create",
                "",
                ALICE,
                create(Some(ids::ROOM_TYPE_CALENDAR)),
            )),
        )
        .await;
    server
        .sync_room(
            &client,
            InvitedRoomBuilder::new(chat).add_state_event(stripped(
                "m.room.create",
                "",
                ALICE,
                create(Some("m.space")),
            )),
        )
        .await;
    server
        .sync_room(
            &client,
            InvitedRoomBuilder::new(untyped).add_state_event(stripped(
                "m.room.create",
                "",
                ALICE,
                create(None),
            )),
        )
        .await;

    let mut listed: Vec<_> = list_calendar_invitations(&client)
        .iter()
        .map(|invitation| invitation.room_id().to_owned())
        .collect();
    listed.sort();
    assert_eq!(listed, vec![stable.to_owned(), unstable.to_owned()]);

    // Invitations are not calendars yet.
    assert!(list_calendars(&client).is_empty());
}

#[tokio::test]
async fn try_from_room_rejects_non_calendars_and_joined_rooms() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let chat = server
        .sync_room(
            &client,
            InvitedRoomBuilder::new(room_id!("!chat:example.org")).add_state_event(stripped(
                "m.room.create",
                "",
                ALICE,
                create(None),
            )),
        )
        .await;
    assert!(matches!(
        CalendarInvitation::try_from_room(chat),
        Err(Error::NotACalendarRoom(_))
    ));

    let joined_create: Raw<AnySyncStateEvent> = Raw::new(&json!({
        "type": "m.room.create",
        "state_key": "",
        "event_id": "$create",
        "sender": ALICE,
        "origin_server_ts": 1,
        "content": create(Some(ids::ROOM_TYPE_CALENDAR_UNSTABLE)),
    }))
    .expect("serializable")
    .cast_unchecked();
    let joined = server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id!("!joined:example.org")).add_state_event(joined_create),
        )
        .await;
    // A joined calendar is a `Calendar`, not an invitation.
    assert!(Calendar::try_from_room(joined.clone()).is_ok());
    assert!(matches!(
        CalendarInvitation::try_from_room(joined),
        Err(Error::Other(_))
    ));
}

// ---------------------------------------------------------------------------------------------
// Accessors on the stripped state
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn reads_name_and_inviter_from_the_stripped_state() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let invitation = invitation(
        &server,
        &client,
        room_id!("!shared:example.org"),
        synapse_invite_state("Alice's calendar"),
    )
    .await;

    assert_eq!(invitation.name().await.as_deref(), Some("Alice's calendar"));
    assert_eq!(invitation.inviter().await.as_deref(), Some(alice()));
    assert!(!invitation.is_encrypted().await);
    assert_eq!(invitation.room().own_user_id(), own());
}

#[tokio::test]
async fn falls_back_to_the_calendar_info_name_without_a_room_name() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let state = vec![
        stripped(
            "m.room.create",
            "",
            ALICE,
            create(Some(ids::ROOM_TYPE_CALENDAR_UNSTABLE)),
        ),
        stripped(
            ids::CALENDAR_INFO_UNSTABLE,
            "",
            ALICE,
            calendar_info_content("From calendar_info", Some("#3366cc")),
        ),
    ];
    let invitation = invitation(&server, &client, room_id!("!noname:example.org"), state).await;

    assert_eq!(
        invitation.name().await.as_deref(),
        Some("From calendar_info")
    );
    assert_eq!(invitation.color().await.as_deref(), Some("#3366cc"));
    assert_eq!(
        invitation.info().await.unwrap().unwrap().info.timezone,
        Some(chrono_tz::Europe::Berlin)
    );
}

#[tokio::test]
async fn no_colour_without_the_calendar_info_in_the_stripped_state() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let invitation = invitation(
        &server,
        &client,
        room_id!("!nocolour:example.org"),
        synapse_invite_state("No colour"),
    )
    .await;

    // What Synapse sends: the name is there, the calendar metadata is not.
    assert_eq!(invitation.color().await, None);
    assert!(invitation.info().await.unwrap().is_none());
}

#[tokio::test]
async fn reads_the_stable_calendar_info_type_too() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let state = vec![
        stripped(
            "m.room.create",
            "",
            ALICE,
            create(Some(ids::ROOM_TYPE_CALENDAR)),
        ),
        stripped(
            ids::CALENDAR_INFO,
            "",
            ALICE,
            calendar_info_content("Stable", Some("#ff0000")),
        ),
    ];
    let invitation = invitation(&server, &client, room_id!("!stable:example.org"), state).await;

    assert_eq!(invitation.color().await.as_deref(), Some("#ff0000"));
}

#[tokio::test]
async fn drops_an_invalid_colour_like_the_joined_path() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let state = vec![
        stripped(
            "m.room.create",
            "",
            ALICE,
            create(Some(ids::ROOM_TYPE_CALENDAR_UNSTABLE)),
        ),
        stripped(
            ids::CALENDAR_INFO_UNSTABLE,
            "",
            ALICE,
            json!({
                "m.text": [{ "body": "Bad colour", "mimetype": "text/plain" }],
                ids::CALENDAR_INFO_UNSTABLE: { "color": "rebeccapurple" },
            }),
        ),
    ];
    let invitation = invitation(&server, &client, room_id!("!badcolour:example.org"), state).await;

    assert_eq!(invitation.color().await, None);
    assert_eq!(invitation.name().await.as_deref(), Some("Bad colour"));
}

#[tokio::test]
async fn is_encrypted_from_the_stripped_encryption_event() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let mut state = synapse_invite_state("Encrypted");
    state.push(stripped(
        "m.room.encryption",
        "",
        ALICE,
        json!({ "algorithm": "m.megolm.v1.aes-sha2" }),
    ));
    let invitation = invitation(&server, &client, room_id!("!e2ee:example.org"), state).await;

    assert!(invitation.is_encrypted().await);
}

// ---------------------------------------------------------------------------------------------
// The role and its fallback
// ---------------------------------------------------------------------------------------------

/// The SDK's power levels for a calendar with a reader default and `user` at `level`.
fn power_levels(user: &str, level: i64) -> Value {
    json!({
        "users": { ALICE: 100, user: level },
        "users_default": 0,
        "events_default": 50,
        "state_default": 100,
        "events": {
            ids::CALENDAR_EVENT_UNSTABLE: 50,
            ids::CALENDAR_EVENT: 50,
            ids::CALENDAR_INFO_UNSTABLE: 100,
            "m.room.power_levels": 100,
        },
        "invite": 100,
        "kick": 100,
        "ban": 100,
        "redact": 100,
    })
}

#[tokio::test]
async fn role_falls_back_to_reader_without_stripped_power_levels() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    // Synapse's stripped state has no `m.room.power_levels`, so the role is unknown and the
    // SDK reports its default role, the least privileged one.
    let invitation = invitation(
        &server,
        &client,
        room_id!("!unknown-role:example.org"),
        synapse_invite_state("Unknown role"),
    )
    .await;

    assert_eq!(invitation.role().await, CalendarRole::Reader);
}

#[tokio::test]
async fn role_from_stripped_power_levels() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    for (level, expected) in [
        (0, CalendarRole::Reader),
        (50, CalendarRole::Editor),
        (100, CalendarRole::Owner),
    ] {
        let mut state = synapse_invite_state("With power levels");
        state.push(stripped(
            "m.room.power_levels",
            "",
            ALICE,
            power_levels(OWN, level),
        ));
        let room_id = format!("!role{level}:example.org");
        let invitation = invitation(
            &server,
            &client,
            &RoomId::parse(&room_id).expect("valid room id"),
            state,
        )
        .await;
        assert_eq!(invitation.role().await, expected, "at power level {level}");
    }
}

#[tokio::test]
async fn role_falls_back_to_reader_for_unparsable_power_levels() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let mut state = synapse_invite_state("Broken power levels");
    state.push(stripped(
        "m.room.power_levels",
        "",
        ALICE,
        json!({ "users_default": "not a number" }),
    ));
    let invitation = invitation(&server, &client, room_id!("!broken:example.org"), state).await;

    assert_eq!(invitation.role().await, CalendarRole::Reader);
}

// ---------------------------------------------------------------------------------------------
// Accepting, declining and leaving
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn accept_joins_and_returns_a_usable_calendar() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let room_id = room_id!("!accept:example.org");
    server
        .mock_room_join(room_id)
        .ok()
        .mock_once()
        .mount()
        .await;

    let invitation = invitation(
        &server,
        &client,
        room_id,
        synapse_invite_state("Accepted calendar"),
    )
    .await;
    let calendar = invitation.accept().await.expect("accept the invitation");

    assert_eq!(calendar.room_id(), room_id);
    assert_eq!(calendar.room().state(), RoomState::Joined);
}

#[tokio::test]
async fn decline_leaves_the_room() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let room_id = room_id!("!decline:example.org");
    server
        .mock_room_leave()
        .ok(room_id)
        .mock_once()
        .mount()
        .await;
    // `matrix-sdk` forgets invites it leaves, so no explicit `forget` call is needed.
    server.mock_room_forget().ok().mock_once().mount().await;

    let invitation = invitation(
        &server,
        &client,
        room_id,
        synapse_invite_state("Declined calendar"),
    )
    .await;
    invitation.decline().await.expect("decline the invitation");

    assert!(list_calendar_invitations(&client).is_empty());
    assert!(client.get_room(room_id).is_none());
}

#[tokio::test]
async fn leave_removes_the_calendar_from_the_list() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let room_id = room_id!("!leave:example.org");
    server
        .mock_room_leave()
        .ok(room_id)
        .mock_once()
        .mount()
        .await;
    server.mock_room_forget().ok().mock_once().mount().await;

    let joined_create: Raw<AnySyncStateEvent> = Raw::new(&json!({
        "type": "m.room.create",
        "state_key": "",
        "event_id": "$create",
        "sender": ALICE,
        "origin_server_ts": 1,
        "content": create(Some(ids::ROOM_TYPE_CALENDAR_UNSTABLE)),
    }))
    .expect("serializable")
    .cast_unchecked();
    let room = server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_state_event(joined_create),
        )
        .await;
    let calendar = Calendar::try_from_room(room).expect("a calendar");
    assert_eq!(list_calendars(&client).len(), 1);

    calendar.leave().await.expect("leave the calendar");

    assert!(list_calendars(&client).is_empty());
    assert!(client.get_room(room_id).is_none());
}
