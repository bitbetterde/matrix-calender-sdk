//! Integration tests for busy/free computation, working hours, tiers and the availability
//! endpoints.

use chrono::{DateTime, NaiveDate, NaiveTime, Utc};
use chrono_tz::{Europe::Berlin, Tz};
use matrix_calendar_sdk::{
    Error,
    availability::{
        compute_busy, compute_busy_from_occurrences, evaluate_tier, get_policy, get_window,
        merge_intervals, query_availability, set_policy, set_window, unavailable_from_windows,
    },
    events::{
        AvailabilityPolicyEventContent, AvailabilityResponse, AvailabilityWindowEventContent,
        BusyInterval, BusyType, CalendarEvent, EventTime, Frequency, Grant, RRule, Recurrence,
        Status, TIER_BUSY, TIER_NONE, TIER_TITLES, Transparency, Weekday, Window,
    },
    recurrence::{ExpandOptions, expand},
};
use matrix_sdk::{
    ruma::{
        event_id,
        events::{AnySyncStateEvent, StateEventType},
        owned_user_id, room_id,
        serde::Raw,
        user_id,
    },
    test_utils::mocks::MatrixMockServer,
};
use matrix_sdk_test::JoinedRoomBuilder;
use serde_json::{Value, json};
use wiremock::{
    Mock, ResponseTemplate,
    matchers::{body_json, header, method, path},
};

fn utc(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
}

fn berlin(local: &str) -> EventTime {
    EventTime::timed(local.parse().unwrap(), Berlin).unwrap()
}

fn event(title: &str, start: &str, end: &str) -> CalendarEvent {
    CalendarEvent::new(
        title,
        berlin(start),
        berlin(end),
        owned_user_id!("@alice:example.com"),
    )
}

fn busy(start: &str, end: &str, kind: BusyType) -> BusyInterval {
    BusyInterval {
        start: utc(start),
        end: utc(end),
        kind,
        title: None,
    }
}

fn unavailable(start: &str, end: &str) -> BusyInterval {
    busy(start, end, BusyType::BusyUnavailable)
}

#[test]
fn compute_busy_merges_clips_and_filters() {
    // 2026-07-01 in Berlin is UTC+2.
    let a = event("A", "2026-07-01T09:00:00", "2026-07-01T10:00:00");
    let b = event("B", "2026-07-01T09:30:00", "2026-07-01T11:00:00");
    let c = event("C", "2026-07-01T11:00:00", "2026-07-01T12:00:00"); // adjacent to B
    let mut transparent = event("Focus", "2026-07-01T13:00:00", "2026-07-01T14:00:00");
    transparent.transparency = Transparency::Transparent;
    let mut cancelled = event("Gone", "2026-07-01T14:00:00", "2026-07-01T15:00:00");
    cancelled.status = Status::Cancelled;
    let mut tentative = event("Maybe", "2026-07-01T15:00:00", "2026-07-01T16:00:00");
    tentative.status = Status::Tentative;
    // Starts before the range: clipped.
    let early = event("Early", "2026-06-30T23:00:00", "2026-07-01T03:00:00");
    // Entirely outside the range.
    let outside = event("Later", "2026-07-03T09:00:00", "2026-07-03T10:00:00");
    // All-day, resolved in the fallback timezone (UTC here).
    let all_day = CalendarEvent::new(
        "Holiday",
        EventTime::all_day(NaiveDate::from_ymd_opt(2026, 7, 2).unwrap()),
        EventTime::all_day(NaiveDate::from_ymd_opt(2026, 7, 3).unwrap()),
        owned_user_id!("@alice:example.com"),
    );

    let events = [
        &tentative,
        &c,
        &a,
        &transparent,
        &cancelled,
        &b,
        &early,
        &outside,
        &all_day,
    ];
    let range_start = utc("2026-07-01T00:00:00Z");
    let range_end = utc("2026-07-03T00:00:00Z");

    let result = compute_busy(events, range_start, range_end, Tz::UTC, false);
    assert_eq!(
        result,
        vec![
            busy(
                "2026-07-01T00:00:00Z",
                "2026-07-01T01:00:00Z",
                BusyType::Busy
            ),
            busy(
                "2026-07-01T07:00:00Z",
                "2026-07-01T10:00:00Z",
                BusyType::Busy
            ),
            busy(
                "2026-07-01T13:00:00Z",
                "2026-07-01T14:00:00Z",
                BusyType::BusyTentative
            ),
            busy(
                "2026-07-02T00:00:00Z",
                "2026-07-03T00:00:00Z",
                BusyType::Busy
            ),
        ]
    );

    // With titles (Tier 2) nothing is merged.
    let titled = compute_busy(events, range_start, range_end, Tz::UTC, true);
    let titles: Vec<_> = titled.iter().map(|i| i.title.as_deref().unwrap()).collect();
    assert_eq!(titles, vec!["Early", "A", "B", "C", "Maybe", "Holiday"]);
    assert_eq!(titled[0].start, range_start);
    assert_eq!(titled[4].kind, BusyType::BusyTentative);

    // The all-day event follows the fallback timezone.
    let in_berlin = compute_busy([&all_day], range_start, range_end, Berlin, false);
    assert_eq!(
        in_berlin,
        vec![busy(
            "2026-07-01T22:00:00Z",
            "2026-07-02T22:00:00Z",
            BusyType::Busy
        )]
    );
}

#[test]
fn merge_intervals_keeps_types_apart() {
    let merged = merge_intervals(vec![
        busy(
            "2026-07-01T10:00:00Z",
            "2026-07-01T11:00:00Z",
            BusyType::Busy,
        ),
        busy(
            "2026-07-01T08:00:00Z",
            "2026-07-01T09:00:00Z",
            BusyType::Busy,
        ),
        busy(
            "2026-07-01T09:00:00Z",
            "2026-07-01T09:30:00Z",
            BusyType::Busy,
        ),
        busy(
            "2026-07-01T08:30:00Z",
            "2026-07-01T12:00:00Z",
            BusyType::BusyTentative,
        ),
        busy(
            "2026-07-01T10:30:00Z",
            "2026-07-01T10:45:00Z",
            BusyType::Busy,
        ),
        busy(
            "2026-07-01T12:00:00Z",
            "2026-07-01T12:00:00Z",
            BusyType::Busy,
        ),
    ]);
    assert_eq!(
        merged,
        vec![
            busy(
                "2026-07-01T08:00:00Z",
                "2026-07-01T09:30:00Z",
                BusyType::Busy
            ),
            busy(
                "2026-07-01T08:30:00Z",
                "2026-07-01T12:00:00Z",
                BusyType::BusyTentative
            ),
            busy(
                "2026-07-01T10:00:00Z",
                "2026-07-01T11:00:00Z",
                BusyType::Busy
            ),
        ]
    );
}

fn weekdays() -> Vec<Weekday> {
    vec![
        Weekday::Monday,
        Weekday::Tuesday,
        Weekday::Wednesday,
        Weekday::Thursday,
        Weekday::Friday,
    ]
}

fn window(days: Vec<Weekday>, start: (u32, u32), end: (u32, u32)) -> Window {
    Window {
        days,
        start_time: NaiveTime::from_hms_opt(start.0, start.1, 0).unwrap(),
        end_time: NaiveTime::from_hms_opt(end.0, end.1, 0).unwrap(),
        timezone: Berlin,
    }
}

#[test]
fn working_hours_across_dst_change() {
    // Week of Mon 2026-03-23 .. Mon 2026-03-30 in Berlin. DST starts on Sunday 2026-03-29 at
    // 02:00 CET (01:00Z), so 09:00 is 08:00Z before and 07:00Z after the change.
    let windows = AvailabilityWindowEventContent::new(vec![
        window(weekdays(), (9, 0), (17, 0)),
        // A Sunday window spanning the change: 01:00 CET (00:00Z) to 04:00 CEST (02:00Z).
        window(vec![Weekday::Sunday], (1, 0), (4, 0)),
    ]);
    let range_start = utc("2026-03-22T23:00:00Z"); // Mon 00:00 CET
    let range_end = utc("2026-03-30T22:00:00Z"); // Tue 00:00 CEST

    assert_eq!(
        unavailable_from_windows(&windows, range_start, range_end),
        vec![
            unavailable("2026-03-22T23:00:00Z", "2026-03-23T08:00:00Z"),
            unavailable("2026-03-23T16:00:00Z", "2026-03-24T08:00:00Z"),
            unavailable("2026-03-24T16:00:00Z", "2026-03-25T08:00:00Z"),
            unavailable("2026-03-25T16:00:00Z", "2026-03-26T08:00:00Z"),
            unavailable("2026-03-26T16:00:00Z", "2026-03-27T08:00:00Z"),
            unavailable("2026-03-27T16:00:00Z", "2026-03-29T00:00:00Z"),
            unavailable("2026-03-29T02:00:00Z", "2026-03-30T07:00:00Z"),
            unavailable("2026-03-30T15:00:00Z", "2026-03-30T22:00:00Z"),
        ]
    );
}

#[test]
fn working_hours_edge_cases() {
    let range_start = utc("2026-03-28T23:00:00Z"); // Sun 2026-03-29 00:00 CET
    let range_end = utc("2026-03-29T22:00:00Z"); // Mon 00:00 CEST

    // A window starting inside the DST gap (02:30 does not exist on 2026-03-29) starts at the
    // instant 02:30 CET would be, i.e. 03:30 CEST (01:30Z).
    let gap =
        AvailabilityWindowEventContent::new(vec![window(vec![Weekday::Sunday], (2, 30), (5, 0))]);
    assert_eq!(
        unavailable_from_windows(&gap, range_start, range_end),
        vec![
            unavailable("2026-03-28T23:00:00Z", "2026-03-29T01:30:00Z"),
            unavailable("2026-03-29T03:00:00Z", "2026-03-29T22:00:00Z"),
        ]
    );

    // An overnight window (22:00-06:00) on Saturday reaches into Sunday.
    let overnight =
        AvailabilityWindowEventContent::new(vec![window(vec![Weekday::Saturday], (22, 0), (6, 0))]);
    assert_eq!(
        unavailable_from_windows(&overnight, range_start, range_end),
        vec![unavailable("2026-03-29T04:00:00Z", "2026-03-29T22:00:00Z")]
    );

    // No windows: nothing is unavailable.
    let none = AvailabilityWindowEventContent::default();
    assert!(unavailable_from_windows(&none, range_start, range_end).is_empty());

    // Windows on other days only: the whole range is unavailable.
    let weekdays_only =
        AvailabilityWindowEventContent::new(vec![window(weekdays(), (9, 0), (17, 0))]);
    assert_eq!(
        unavailable_from_windows(&weekdays_only, range_start, range_end),
        vec![unavailable("2026-03-28T23:00:00Z", "2026-03-29T22:00:00Z")]
    );
}

#[test]
fn tier_evaluation() {
    let policy = AvailabilityPolicyEventContent::new(
        TIER_NONE,
        vec![
            Grant::new("@bob:example.com", TIER_BUSY),
            Grant::new("*:trusted-corp.example", TIER_BUSY),
            Grant::new("@carol:trusted-corp.example", TIER_TITLES),
        ],
    );
    assert_eq!(
        evaluate_tier(&policy, user_id!("@bob:example.com")),
        TIER_BUSY
    );
    assert_eq!(
        evaluate_tier(&policy, user_id!("@dave:trusted-corp.example")),
        TIER_BUSY
    );
    // Exact and wildcard grants match; the highest wins.
    assert_eq!(
        evaluate_tier(&policy, user_id!("@carol:trusted-corp.example")),
        TIER_TITLES
    );
    // The wildcard matches the whole server name only.
    assert_eq!(
        evaluate_tier(&policy, user_id!("@eve:evil-trusted-corp.example")),
        TIER_NONE
    );
    assert_eq!(
        evaluate_tier(&policy, user_id!("@eve:example.com")),
        TIER_NONE
    );

    // Default tier applies without a matching grant; an explicit grant overrides it.
    let open = AvailabilityPolicyEventContent::new(
        TIER_BUSY,
        vec![Grant::new("@mallory:example.com", TIER_NONE)],
    );
    assert_eq!(
        evaluate_tier(&open, user_id!("@bob:example.com")),
        TIER_BUSY
    );
    assert_eq!(
        evaluate_tier(&open, user_id!("@mallory:example.com")),
        TIER_NONE
    );
}

fn msc_response() -> Value {
    json!({
      "user_id": "@alice:example.com",
      "range_start": "2026-07-01T00:00:00Z",
      "range_end": "2026-07-07T23:59:59Z",
      "busy": [
        { "start": "2026-07-01T07:00:00Z", "end": "2026-07-01T07:30:00Z", "type": "BUSY" },
        { "start": "2026-07-03T08:00:00Z", "end": "2026-07-03T17:00:00Z", "type": "BUSY-UNAVAILABLE" }
      ]
    })
}

const AVAILABILITY_PATH: &str =
    "/_matrix/client/unstable/org.matrix.msc4496/user/@alice:example.com/availability";

#[tokio::test]
async fn query_availability_unstable_endpoint() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    Mock::given(method("POST"))
        .and(path(AVAILABILITY_PATH))
        .and(header("authorization", "Bearer 1234"))
        .and(body_json(json!({
            "range_start": "2026-07-01T00:00:00Z",
            "range_end": "2026-07-07T23:59:59Z"
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(msc_response()))
        .expect(1)
        .mount(server.server())
        .await;

    let response = query_availability(
        &client,
        user_id!("@alice:example.com"),
        utc("2026-07-01T00:00:00Z"),
        utc("2026-07-07T23:59:59Z"),
    )
    .await
    .unwrap();
    let expected: AvailabilityResponse = serde_json::from_value(msc_response()).unwrap();
    assert_eq!(response, expected);
    assert_eq!(response.busy[1].kind, BusyType::BusyUnavailable);
}

#[tokio::test]
async fn query_availability_forbidden() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    Mock::given(method("POST"))
        .and(path(AVAILABILITY_PATH))
        .respond_with(ResponseTemplate::new(403).set_body_json(json!({
            "errcode": "M_FORBIDDEN",
            "error": "You are not allowed to see this user's availability"
        })))
        .mount(server.server())
        .await;

    let err = query_availability(
        &client,
        user_id!("@alice:example.com"),
        utc("2026-07-01T00:00:00Z"),
        utc("2026-07-07T23:59:59Z"),
    )
    .await
    .unwrap_err();
    let Error::PermissionDenied(message) = err else {
        panic!("expected PermissionDenied, got {err:?}");
    };
    assert!(message.contains("not allowed"), "{message}");

    // Other errors are passed through.
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    Mock::given(method("POST"))
        .and(path(AVAILABILITY_PATH))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({
            "errcode": "M_UNRECOGNIZED",
            "error": "Unrecognized request"
        })))
        .mount(server.server())
        .await;
    let err = query_availability(
        &client,
        user_id!("@alice:example.com"),
        utc("2026-07-01T00:00:00Z"),
        utc("2026-07-07T23:59:59Z"),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, Error::Http(_)), "{err:?}");
}

fn state_event(event_type: &str, ts: u64, content: Value) -> Raw<AnySyncStateEvent> {
    Raw::from_json_string(
        json!({
            "type": event_type,
            "state_key": "",
            "event_id": format!("$state-{event_type}-{ts}"),
            "sender": "@alice:example.com",
            "origin_server_ts": ts,
            "content": content,
        })
        .to_string(),
    )
    .unwrap()
}

#[tokio::test]
async fn policy_and_window_state_events() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let room_id = room_id!("!calendar:example.com");

    let policy_json = json!({
        "default_tier": 0,
        "grants": [
            { "subject": "@bob:example.com", "tier": 1 },
            { "subject": "*:trusted-corp.example", "tier": 1 }
        ]
    });
    let window_json = json!({
        "windows": [{
            "days": ["MO", "TU", "WE", "TH", "FR"],
            "start_time": "09:00",
            "end_time": "17:00",
            "timezone": "Europe/Berlin"
        }]
    });

    // The stable policy is newer than the unstable one; the window only exists in stable form.
    let room = server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_state_event(state_event(
                    "org.matrix.msc4496.availability_policy",
                    1000,
                    json!({ "default_tier": 2 }),
                ))
                .add_state_event(state_event(
                    "m.calendar.availability_policy",
                    2000,
                    policy_json.clone(),
                ))
                .add_state_event(state_event(
                    "m.calendar.availability_window",
                    1000,
                    window_json.clone(),
                )),
        )
        .await;

    let policy = get_policy(&room).await.unwrap().unwrap();
    assert_eq!(policy, serde_json::from_value(policy_json.clone()).unwrap());
    let window = get_window(&room).await.unwrap().unwrap();
    assert_eq!(window.windows[0].timezone, Berlin);
    assert_eq!(window.windows[0].days, weekdays());

    // A room without the events.
    let empty = server
        .sync_joined_room(&client, room_id!("!other:example.com"))
        .await;
    assert!(get_policy(&empty).await.unwrap().is_none());
    assert!(get_window(&empty).await.unwrap().is_none());

    // Writing uses the unstable type.
    server
        .mock_room_send_state()
        .for_type(StateEventType::from(
            "org.matrix.msc4496.availability_policy",
        ))
        .ok(event_id!("$policy"))
        .expect(1)
        .mount()
        .await;
    server
        .mock_room_send_state()
        .for_type(StateEventType::from(
            "org.matrix.msc4496.availability_window",
        ))
        .ok(event_id!("$window"))
        .expect(1)
        .mount()
        .await;
    assert_eq!(set_policy(&room, policy).await.unwrap(), "$policy");
    assert_eq!(set_window(&room, window).await.unwrap(), "$window");

    let requests = server.server().received_requests().await.unwrap();
    let body_for = |needle: &str| -> Value {
        let request = requests
            .iter()
            .find(|r| r.method.as_str() == "PUT" && r.url.path().contains(needle))
            .unwrap();
        serde_json::from_slice(&request.body).unwrap()
    };
    assert_eq!(
        body_for("/state/org.matrix.msc4496.availability_policy"),
        policy_json
    );
    assert_eq!(
        body_for("/state/org.matrix.msc4496.availability_window"),
        window_json
    );
}

#[test]
fn compute_busy_skips_series_and_occurrences_are_counted() {
    // Daily 09:00-10:00 Berlin, three instances; the second is moved and the third cancelled.
    let mut series = event("Daily", "2026-07-01T09:00:00", "2026-07-01T10:00:00");
    series.recurrence = Some(Recurrence::new(RRule {
        count: Some(3),
        ..RRule::new(Frequency::Daily)
    }));
    let mut moved = event("Moved", "2026-07-02T14:00:00", "2026-07-02T15:00:00");
    moved.uid = series.uid.clone();
    moved.recurrence_id = Some(utc("2026-07-02T07:00:00Z"));
    moved.status = Status::Tentative;
    let mut cancelled = event("Daily", "2026-07-03T09:00:00", "2026-07-03T10:00:00");
    cancelled.uid = series.uid.clone();
    cancelled.recurrence_id = Some(utc("2026-07-03T07:00:00Z"));
    cancelled.status = Status::Cancelled;
    let one_off = event("One-off", "2026-07-01T12:00:00", "2026-07-01T13:00:00");

    let range_start = utc("2026-07-01T00:00:00Z");
    let range_end = utc("2026-07-05T00:00:00Z");

    // The master is not treated as a single instance; only the one-off counts.
    assert_eq!(
        compute_busy([&series, &one_off], range_start, range_end, Tz::UTC, false),
        vec![busy(
            "2026-07-01T10:00:00Z",
            "2026-07-01T11:00:00Z",
            BusyType::Busy
        )]
    );

    let occurrences = expand(
        &series,
        [&moved, &cancelled],
        &ExpandOptions::new(range_start, range_end),
    )
    .unwrap();
    assert_eq!(
        compute_busy_from_occurrences(&occurrences, range_start, range_end, Tz::UTC, false),
        vec![
            busy(
                "2026-07-01T07:00:00Z",
                "2026-07-01T08:00:00Z",
                BusyType::Busy
            ),
            busy(
                "2026-07-02T12:00:00Z",
                "2026-07-02T13:00:00Z",
                BusyType::BusyTentative
            ),
        ]
    );
    let titled = compute_busy_from_occurrences(&occurrences, range_start, range_end, Tz::UTC, true);
    let titles: Vec<_> = titled.iter().map(|i| i.title.as_deref().unwrap()).collect();
    assert_eq!(titles, ["Daily", "Moved"]);
}

#[test]
fn unknown_busy_types_round_trip() {
    let interval: BusyInterval = serde_json::from_value(json!({
        "start": "2026-07-01T07:00:00Z",
        "end": "2026-07-01T08:00:00Z",
        "type": "X-OUT-OF-OFFICE"
    }))
    .unwrap();
    assert_eq!(interval.kind, BusyType::Other("X-OUT-OF-OFFICE".into()));
    assert_eq!(
        serde_json::to_value(&interval).unwrap()["type"],
        "X-OUT-OF-OFFICE"
    );
    // Unknown types are kept separate when merging.
    let merged = merge_intervals(vec![
        interval.clone(),
        busy(
            "2026-07-01T07:30:00Z",
            "2026-07-01T09:00:00Z",
            BusyType::Busy,
        ),
        interval.clone(),
    ]);
    assert_eq!(merged.len(), 2);
}
