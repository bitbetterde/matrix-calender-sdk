//! Tests for `LiveCalendar`: the index loads the history and then follows sync.

mod common;

use std::time::Duration;

use common::*;
use matrix_calendar_sdk::calendar::{Calendar, EventKey, IndexChange, LoadOptions};
use matrix_sdk::{
    ruma::{event_id, room_id},
    test_utils::mocks::{MatrixMockServer, RoomMessagesResponseTemplate},
};
use tokio::sync::broadcast::error::TryRecvError;

fn key(uid: &str) -> EventKey {
    EventKey::new(alice().to_owned(), uid)
}

#[tokio::test]
async fn live_index_follows_sync() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let room_id = room_id!("!cal:example.org");
    let room = server.sync_room(&client, calendar_room(room_id)).await;
    let calendar = Calendar::try_from_room(room).unwrap();

    server
        .mock_room_messages()
        .ok(
            RoomMessagesResponseTemplate::default().events(vec![original_in_room(
                room_id,
                alice(),
                event_id!("$lunch"),
                10,
                event(
                    "lunch",
                    "Lunch",
                    alice(),
                    "2026-07-09T12:00:00",
                    "2026-07-09T13:00:00",
                ),
            )]),
        )
        .mount()
        .await;

    let live = calendar.live(LoadOptions::default()).await.unwrap();
    assert_eq!(live.calendar().room_id(), room_id);
    assert_eq!(live.read().await.len(), 1);
    let mut changes = live.subscribe();

    // A new series arrives through sync.
    server
        .sync_room(
            &client,
            calendar_room(room_id).add_timeline_event(original(
                alice(),
                event_id!("$series"),
                20,
                weekly_standup(alice()),
            )),
        )
        .await;
    assert_eq!(
        changes.try_recv().unwrap(),
        IndexChange {
            key: key("standup-uid")
        }
    );
    {
        let index = live.read().await;
        assert_eq!(index.len(), 2);
        assert_eq!(
            index.get(&key("standup-uid")).unwrap().event_id,
            event_id!("$series")
        );
    }

    // An edit of the history event, plus an unrelated message and a re-delivered event, in
    // one sync response: only the edit is a change.
    let mut late = event(
        "lunch",
        "Late lunch",
        alice(),
        "2026-07-09T13:00:00",
        "2026-07-09T14:00:00",
    );
    late.description = Some("moved".into());
    server
        .sync_room(
            &client,
            calendar_room(room_id)
                .add_timeline_event(
                    factory(alice())
                        .text_msg("hello")
                        .event_id(event_id!("$text"))
                        .server_ts(25)
                        .into_raw_sync(),
                )
                .add_timeline_event(edit(
                    alice(),
                    event_id!("$lunch-edit"),
                    30,
                    event_id!("$lunch"),
                    late,
                ))
                .add_timeline_event(original(
                    alice(),
                    event_id!("$series"),
                    20,
                    weekly_standup(alice()),
                )),
        )
        .await;
    assert_eq!(
        changes.try_recv().unwrap(),
        IndexChange { key: key("lunch") }
    );
    assert_eq!(changes.try_recv(), Err(TryRecvError::Empty));
    assert_eq!(
        live.read().await.get(&key("lunch")).unwrap().content.title,
        "Late lunch"
    );

    // Occurrence queries work on the live index.
    let occurrences = live
        .read()
        .await
        .occurrences(
            utc("2026-07-01T00:00:00Z"),
            utc("2026-07-10T00:00:00Z"),
            chrono_tz::Europe::Berlin,
        )
        .unwrap();
    let uids: Vec<_> = occurrences.iter().map(|o| o.key.uid.as_str()).collect();
    assert_eq!(uids, ["standup-uid", "standup-uid", "lunch"]);

    // Events in other rooms are not applied.
    server
        .sync_room(
            &client,
            calendar_room(room_id!("!other:example.org")).add_timeline_event(original(
                alice(),
                event_id!("$elsewhere"),
                40,
                event(
                    "elsewhere",
                    "Elsewhere",
                    alice(),
                    "2026-07-09T12:00:00",
                    "2026-07-09T13:00:00",
                ),
            )),
        )
        .await;
    assert_eq!(changes.try_recv(), Err(TryRecvError::Empty));
    assert_eq!(live.read().await.len(), 2);

    // After dropping the live calendar, the channel closes.
    drop(live);
    assert_eq!(changes.try_recv(), Err(TryRecvError::Closed));
}

#[tokio::test]
async fn events_synced_during_load_are_not_lost() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let room_id = room_id!("!cal:example.org");
    let room = server.sync_room(&client, calendar_room(room_id)).await;
    let calendar = Calendar::try_from_room(room).unwrap();

    // The history: the lunch event and an edit that the live event below supersedes.
    server
        .mock_room_messages()
        .ok(RoomMessagesResponseTemplate::default()
            .events(vec![original_in_room(
                room_id,
                alice(),
                event_id!("$lunch"),
                10,
                event(
                    "lunch",
                    "Lunch",
                    alice(),
                    "2026-07-09T12:00:00",
                    "2026-07-09T13:00:00",
                ),
            )])
            .with_delay(Duration::from_millis(300)))
        .mount()
        .await;

    let live_future = calendar.live(LoadOptions::default());
    fn assert_send<T: Send>(_: &T) {}
    assert_send(&live_future);

    let sync_during_load = async {
        // Wait until the history request is in flight, then deliver a live event: an edit of
        // an event that the history has not returned yet, and a new event.
        loop {
            let requests = server.server().received_requests().await.unwrap();
            if requests.iter().any(|r| r.url.path().ends_with("/messages")) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        server
            .sync_room(
                &client,
                calendar_room(room_id)
                    .add_timeline_event(edit(
                        alice(),
                        event_id!("$lunch-edit"),
                        30,
                        event_id!("$lunch"),
                        event(
                            "lunch",
                            "Late lunch",
                            alice(),
                            "2026-07-09T13:00:00",
                            "2026-07-09T14:00:00",
                        ),
                    ))
                    .add_timeline_event(original(
                        alice(),
                        event_id!("$series"),
                        20,
                        weekly_standup(alice()),
                    )),
            )
            .await;
    };

    let (live, ()) = tokio::join!(live_future, sync_during_load);
    let live = live.unwrap();
    let index = live.read().await;
    assert_eq!(index.len(), 2);
    let lunch = index.get(&key("lunch")).unwrap();
    assert_eq!(lunch.content.title, "Late lunch");
    assert_eq!(
        lunch.last_edit_event_id.as_deref(),
        Some(event_id!("$lunch-edit"))
    );
    assert!(index.get(&key("standup-uid")).is_some());
}
