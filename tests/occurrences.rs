//! Tests for occurrence and busy-time queries on `CalendarIndex` and `Calendar`, and for
//! per-occurrence overrides.

mod common;

use chrono_tz::{Europe::Berlin, Tz};
use common::*;
use matrix_calendar_sdk::{
    calendar::{Calendar, CalendarIndex, CalendarOccurrence, EventKey},
    events::{
        BusyInterval, BusyType, CalendarEventEventContent, EventTime, Frequency, RRule, Recurrence,
        Status, Transparency,
    },
    ids,
    recurrence::{self, ExpandOptions},
};
use matrix_sdk::{
    ruma::{event_id, events::MessageLikeEventType, room_id},
    test_utils::mocks::{MatrixMockServer, RoomMessagesResponseTemplate},
};
use matrix_sdk_test::event_factory::EventFactory;
use serde_json::{Value, json};

/// The index used by most tests (all events by `organizer`):
///
/// - `standup-uid`: weekly on Wednesdays 09:00–09:30 Berlin from 2026-07-01, with the
///   2026-07-08 instance moved to 10:00 (`$o1`) and the 2026-07-15 instance cancelled (`$o2`),
/// - `lunch`: 2026-07-09 12:00–13:00 Berlin, tentative,
/// - `holiday`: all-day on 2026-07-10,
/// - `cancelled-single`: a cancelled one-off event on 2026-07-02,
/// - `cancelled-series`: a cancelled weekly series from 2026-07-03,
/// - `bad-rule`: a series with an invalid recurrence rule (`BYMONTH=13`).
fn sample_index(organizer: &matrix_sdk::ruma::UserId) -> CalendarIndex {
    let mut index = CalendarIndex::new();
    let mut apply = |raw| {
        index.apply_raw_sync(&raw).expect("well-formed event");
    };

    apply(original(
        organizer,
        event_id!("$series"),
        10,
        weekly_standup(organizer),
    ));

    let mut moved = event(
        "standup-uid",
        "Late standup",
        organizer,
        "2026-07-08T10:00:00",
        "2026-07-08T10:30:00",
    );
    moved.recurrence_id = Some(utc("2026-07-08T07:00:00Z"));
    apply(override_event(
        organizer,
        event_id!("$o1"),
        20,
        event_id!("$series"),
        moved,
    ));

    let mut cancelled = event(
        "standup-uid",
        "Team standup",
        organizer,
        "2026-07-15T09:00:00",
        "2026-07-15T09:30:00",
    );
    cancelled.recurrence_id = Some(utc("2026-07-15T07:00:00Z"));
    cancelled.status = Status::Cancelled;
    apply(override_event(
        organizer,
        event_id!("$o2"),
        30,
        event_id!("$series"),
        cancelled,
    ));

    let mut lunch = event(
        "lunch",
        "Lunch",
        organizer,
        "2026-07-09T12:00:00",
        "2026-07-09T13:00:00",
    );
    lunch.status = Status::Tentative;
    apply(original(organizer, event_id!("$lunch"), 40, lunch));

    apply(original(
        organizer,
        event_id!("$holiday"),
        50,
        all_day("holiday", "Holiday", organizer, "2026-07-10"),
    ));

    let mut cancelled_single = event(
        "cancelled-single",
        "Cancelled",
        organizer,
        "2026-07-02T09:00:00",
        "2026-07-02T10:00:00",
    );
    cancelled_single.status = Status::Cancelled;
    apply(original(
        organizer,
        event_id!("$cancelled-single"),
        60,
        cancelled_single,
    ));

    let mut cancelled_series = event(
        "cancelled-series",
        "Cancelled series",
        organizer,
        "2026-07-03T09:00:00",
        "2026-07-03T10:00:00",
    );
    cancelled_series.recurrence = Some(Recurrence::new(RRule::new(Frequency::Weekly)));
    cancelled_series.status = Status::Cancelled;
    apply(original(
        organizer,
        event_id!("$cancelled-series"),
        70,
        cancelled_series,
    ));

    let mut bad = event(
        "bad-rule",
        "Bad rule",
        organizer,
        "2026-07-04T09:00:00",
        "2026-07-04T10:00:00",
    );
    let mut rule = RRule::new(Frequency::Monthly);
    rule.bymonth = Some(vec![13]);
    bad.recurrence = Some(Recurrence::new(rule));
    apply(original(organizer, event_id!("$bad"), 80, bad));

    index
}

fn summary(occurrences: &[CalendarOccurrence], tz: Tz) -> Vec<(String, String, String)> {
    occurrences
        .iter()
        .map(|o| {
            (
                o.key.uid.clone(),
                o.occurrence.event.title.clone(),
                o.start_utc(tz).to_rfc3339(),
            )
        })
        .collect()
}

#[test]
fn occurrences_expand_series_apply_overrides_and_skip_cancelled() {
    let index = sample_index(alice());
    assert_eq!(index.len(), 6);

    let occurrences = index
        .occurrences(
            utc("2026-06-29T00:00:00Z"),
            utc("2026-07-27T00:00:00Z"),
            Berlin,
        )
        .unwrap();

    assert_eq!(
        summary(&occurrences, Berlin),
        vec![
            (
                "standup-uid".into(),
                "Team standup".into(),
                "2026-07-01T07:00:00+00:00".into()
            ),
            // The override's times and content are used.
            (
                "standup-uid".into(),
                "Late standup".into(),
                "2026-07-08T08:00:00+00:00".into()
            ),
            (
                "lunch".into(),
                "Lunch".into(),
                "2026-07-09T10:00:00+00:00".into()
            ),
            // All-day: midnight in the fallback timezone.
            (
                "holiday".into(),
                "Holiday".into(),
                "2026-07-09T22:00:00+00:00".into()
            ),
            // 2026-07-15 is cancelled.
            (
                "standup-uid".into(),
                "Team standup".into(),
                "2026-07-22T07:00:00+00:00".into()
            ),
        ]
    );

    // Provenance.
    let first = &occurrences[0];
    assert_eq!(first.key, EventKey::new(alice().to_owned(), "standup-uid"));
    assert_eq!(first.event_id, event_id!("$series"));
    assert_eq!(first.override_event_id, None);
    assert_eq!(
        first.occurrence.recurrence_id,
        Some(utc("2026-07-01T07:00:00Z"))
    );
    assert!(!first.occurrence.is_override);

    let moved = &occurrences[1];
    assert_eq!(moved.event_id, event_id!("$series"));
    assert_eq!(moved.override_event_id.as_deref(), Some(event_id!("$o1")));
    assert!(moved.occurrence.is_override);
    assert_eq!(
        moved.occurrence.recurrence_id,
        Some(utc("2026-07-08T07:00:00Z"))
    );
    assert_eq!(moved.end_utc(Berlin), utc("2026-07-08T08:30:00Z"));

    // Normal instances carry their own times, not the master's.
    let last = occurrences.last().unwrap();
    assert_eq!(last.end_utc(Berlin), utc("2026-07-22T07:30:00Z"));
    assert_eq!(
        last.clone().into_instance().start,
        EventTime::timed(local("2026-07-22T09:00:00"), Berlin).unwrap()
    );

    let lunch = &occurrences[2];
    assert_eq!(lunch.event_id, event_id!("$lunch"));
    assert_eq!(lunch.occurrence.recurrence_id, None);

    // With UTC as fallback the all-day event starts at midnight UTC.
    let utc_occurrences = index
        .occurrences(
            utc("2026-07-09T00:00:00Z"),
            utc("2026-07-11T00:00:00Z"),
            Tz::UTC,
        )
        .unwrap();
    let holiday = utc_occurrences
        .iter()
        .find(|o| o.key.uid == "holiday")
        .unwrap();
    assert_eq!(holiday.start_utc(Tz::UTC), utc("2026-07-10T00:00:00Z"));

    // An empty range yields nothing.
    assert!(
        index
            .occurrences(
                utc("2026-07-01T00:00:00Z"),
                utc("2026-07-01T00:00:00Z"),
                Berlin
            )
            .unwrap()
            .is_empty()
    );
}

#[test]
fn series_with_invalid_rule_is_skipped() {
    let index = sample_index(alice());

    // The rule really is invalid ...
    let bad = index
        .get(&EventKey::new(alice().to_owned(), "bad-rule"))
        .unwrap();
    let opts = ExpandOptions::new(utc("2026-07-01T00:00:00Z"), utc("2026-08-01T00:00:00Z"));
    assert!(recurrence::expand(&bad.content, &[], &opts).is_err());

    // ... but the query still succeeds without it.
    let occurrences = index
        .occurrences(
            utc("2026-07-01T00:00:00Z"),
            utc("2026-08-01T00:00:00Z"),
            Berlin,
        )
        .unwrap();
    assert!(!occurrences.is_empty());
    assert!(occurrences.iter().all(|o| o.key.uid != "bad-rule"));

    // A lone bad series yields an empty result, not an error.
    let mut index = CalendarIndex::new();
    let mut ev = event(
        "bad",
        "Bad",
        alice(),
        "2026-07-04T09:00:00",
        "2026-07-04T10:00:00",
    );
    let mut rule = RRule::new(Frequency::Daily);
    rule.interval = Some(0);
    ev.recurrence = Some(Recurrence::new(rule));
    index
        .apply_raw_sync(&original(alice(), event_id!("$bad"), 1, ev))
        .unwrap();
    assert_eq!(index.len(), 1);
    assert!(
        index
            .occurrences(
                utc("2026-07-01T00:00:00Z"),
                utc("2026-08-01T00:00:00Z"),
                Berlin
            )
            .unwrap()
            .is_empty()
    );
    assert!(
        index
            .next_occurrence(utc("2026-07-01T00:00:00Z"), Berlin)
            .unwrap()
            .is_none()
    );
}

#[test]
fn next_occurrence_across_events() {
    let index = sample_index(alice());

    let next = index
        .next_occurrence(utc("2026-06-01T00:00:00Z"), Berlin)
        .unwrap()
        .unwrap();
    assert_eq!(next.key.uid, "standup-uid");
    assert_eq!(next.start_utc(Berlin), utc("2026-07-01T07:00:00Z"));

    // Strictly after: the 07:00 instance itself is not returned; the override is next.
    let next = index
        .next_occurrence(utc("2026-07-08T07:00:00Z"), Berlin)
        .unwrap()
        .unwrap();
    assert_eq!(next.override_event_id.as_deref(), Some(event_id!("$o1")));
    assert_eq!(next.start_utc(Berlin), utc("2026-07-08T08:00:00Z"));

    let next = index
        .next_occurrence(utc("2026-07-09T12:00:00Z"), Berlin)
        .unwrap()
        .unwrap();
    assert_eq!(next.key.uid, "holiday");

    // The cancelled 2026-07-15 instance and the cancelled series are skipped.
    let next = index
        .next_occurrence(utc("2026-07-14T00:00:00Z"), Berlin)
        .unwrap()
        .unwrap();
    assert_eq!(next.key.uid, "standup-uid");
    assert_eq!(next.start_utc(Berlin), utc("2026-07-22T07:00:00Z"));

    assert!(
        CalendarIndex::new()
            .next_occurrence(utc("2026-07-14T00:00:00Z"), Berlin)
            .unwrap()
            .is_none()
    );
}

#[test]
fn busy_uses_expanded_occurrences() {
    let mut index = sample_index(alice());
    let mut free = event(
        "free",
        "Free",
        alice(),
        "2026-07-08T12:00:00",
        "2026-07-08T13:00:00",
    );
    free.transparency = Transparency::Transparent;
    index
        .apply_raw_sync(&original(alice(), event_id!("$free"), 90, free))
        .unwrap();

    let start = utc("2026-07-08T00:00:00Z");
    let end = utc("2026-07-10T00:00:00Z");
    let busy = index.busy(start, end, Berlin, false).unwrap();
    assert_eq!(
        busy,
        vec![
            BusyInterval {
                start: utc("2026-07-08T08:00:00Z"),
                end: utc("2026-07-08T08:30:00Z"),
                kind: BusyType::Busy,
                title: None,
            },
            BusyInterval {
                start: utc("2026-07-09T10:00:00Z"),
                end: utc("2026-07-09T11:00:00Z"),
                kind: BusyType::BusyTentative,
                title: None,
            },
            // The all-day event, clipped to the range.
            BusyInterval {
                start: utc("2026-07-09T22:00:00Z"),
                end: utc("2026-07-10T00:00:00Z"),
                kind: BusyType::Busy,
                title: None,
            },
        ]
    );

    let titles: Vec<_> = index
        .busy(start, end, Berlin, true)
        .unwrap()
        .into_iter()
        .map(|b| b.title.unwrap())
        .collect();
    assert_eq!(titles, ["Late standup", "Lunch", "Holiday"]);

    // Series instances other than the first are busy at their own times.
    let busy = index
        .busy(
            utc("2026-07-22T00:00:00Z"),
            utc("2026-07-23T00:00:00Z"),
            Berlin,
            false,
        )
        .unwrap();
    assert_eq!(busy.len(), 1);
    assert_eq!(busy[0].start, utc("2026-07-22T07:00:00Z"));
    assert_eq!(busy[0].end, utc("2026-07-22T07:30:00Z"));

    // The cancelled instance is not busy.
    assert!(
        index
            .busy(
                utc("2026-07-15T00:00:00Z"),
                utc("2026-07-16T00:00:00Z"),
                Berlin,
                false
            )
            .unwrap()
            .is_empty()
    );
}

// ---------------------------------------------------------------------------------------------
// Calendar (mocked homeserver)
// ---------------------------------------------------------------------------------------------

fn sent_bodies(requests: &[wiremock::Request]) -> Vec<Value> {
    requests
        .iter()
        .filter(|r| r.method.as_str() == "PUT" && r.url.path().contains("/send/"))
        .map(|r| r.body_json().unwrap())
        .collect()
}

#[tokio::test]
async fn calendar_occurrences_use_info_timezone() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let room_id = room_id!("!cal:example.org");
    let room = server
        .sync_room(
            &client,
            calendar_room(room_id).add_state_event(info_state(Berlin)),
        )
        .await;
    let calendar = Calendar::try_from_room(room).unwrap();
    assert_eq!(calendar.default_timezone().await.unwrap(), Berlin);

    server
        .mock_room_messages()
        .ok(
            RoomMessagesResponseTemplate::default().events(vec![original_in_room(
                room_id,
                alice(),
                event_id!("$holiday"),
                10,
                all_day("holiday", "Holiday", alice(), "2026-07-10"),
            )]),
        )
        .mount()
        .await;

    // 2026-07-10 starts at 22:00 UTC the day before in Berlin.
    let occurrences = calendar
        .occurrences(utc("2026-07-09T22:00:00Z"), utc("2026-07-09T23:00:00Z"))
        .await
        .unwrap();
    assert_eq!(occurrences.len(), 1);
    assert_eq!(occurrences[0].event_id, event_id!("$holiday"));

    // Without calendar_info the fallback is UTC.
    let room = server
        .sync_room(&client, calendar_room(room_id!("!utc:example.org")))
        .await;
    let calendar = Calendar::try_from_room(room).unwrap();
    assert_eq!(calendar.default_timezone().await.unwrap(), Tz::UTC);
    assert!(
        calendar
            .occurrences(utc("2026-07-09T22:00:00Z"), utc("2026-07-09T23:00:00Z"))
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn override_and_cancel_single_occurrences() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let room_id = room_id!("!cal:example.org");
    let room = server.sync_room(&client, calendar_room(room_id)).await;
    let calendar = Calendar::try_from_room(room).unwrap();

    let index = sample_index(own());
    let occurrences = index
        .occurrences(
            utc("2026-06-29T00:00:00Z"),
            utc("2026-07-27T00:00:00Z"),
            Berlin,
        )
        .unwrap();
    let plain = occurrences
        .iter()
        .find(|o| o.start_utc(Berlin) == utc("2026-07-22T07:00:00Z"))
        .unwrap();
    let overridden = occurrences
        .iter()
        .find(|o| o.override_event_id.is_some())
        .unwrap();
    let single = occurrences.iter().find(|o| o.key.uid == "lunch").unwrap();

    // The original series and override are fetched to check the sender.
    let f = EventFactory::new().room(room_id).sender(own());
    server
        .mock_room_event()
        .room(room_id)
        .match_event_id()
        .ok(f
            .event(CalendarEventEventContent::new(weekly_standup(own())))
            .event_id(event_id!("$series"))
            .server_ts(10)
            .into_event())
        .mount()
        .await;
    let mut moved = overridden.clone().into_instance();
    moved.recurrence_id = overridden.occurrence.recurrence_id;
    server
        .mock_room_event()
        .room(room_id)
        .match_event_id()
        .ok(f
            .event(CalendarEventEventContent::override_of(
                event_id!("$series").to_owned(),
                moved,
            ))
            .event_id(event_id!("$o1"))
            .server_ts(20)
            .into_event())
        .mount()
        .await;
    server.mock_room_state_encryption().plain().mount().await;
    server
        .mock_room_send()
        .for_type(MessageLikeEventType::from(ids::CALENDAR_EVENT_UNSTABLE))
        .ok(event_id!("$sent"))
        .mount()
        .await;

    // A plain instance gets a new override.
    let id = calendar
        .create_override_for(plain, |ev| ev.title = "Special standup".into())
        .await
        .unwrap();
    assert_eq!(id, event_id!("$sent"));

    // An overridden instance gets its override edited.
    calendar.cancel_occurrence(overridden).await.unwrap();

    // Non-recurring events have no occurrences to override.
    assert!(calendar.create_override_for(single, |_| {}).await.is_err());
    // The recurrence_id cannot be changed.
    assert!(
        calendar
            .create_override_for(plain, |ev| ev.recurrence_id = None)
            .await
            .is_err()
    );

    let bodies = sent_bodies(&server.server().received_requests().await.unwrap());
    assert_eq!(bodies.len(), 2);

    let created = &bodies[0];
    assert_eq!(
        created["m.relates_to"],
        json!({ "rel_type": ids::CALENDAR_OVERRIDE_REL_TYPE_UNSTABLE, "event_id": "$series" })
    );
    let block = &created[ids::CALENDAR_EVENT_UNSTABLE];
    assert_eq!(block["uid"], "standup-uid");
    assert_eq!(block["title"], "Special standup");
    assert_eq!(block["recurrence_id"], "2026-07-22T07:00:00Z");
    assert_eq!(block["start"]["utc"], "2026-07-22T07:00:00Z");
    assert_eq!(block["end"]["utc"], "2026-07-22T07:30:00Z");
    assert_eq!(block["recurrence"], Value::Null);

    let cancel = &bodies[1];
    assert_eq!(
        cancel["m.relates_to"],
        json!({ "rel_type": "m.replace", "event_id": "$o1" })
    );
    let new_block = &cancel["m.new_content"][ids::CALENDAR_EVENT_UNSTABLE];
    assert_eq!(new_block["status"], "cancelled");
    assert_eq!(new_block["title"], "Late standup");
    assert_eq!(new_block["recurrence_id"], "2026-07-08T07:00:00Z");
    assert_eq!(new_block["start"]["utc"], "2026-07-08T08:00:00Z");
}
