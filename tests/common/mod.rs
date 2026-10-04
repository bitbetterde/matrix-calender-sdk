//! Helpers shared by the occurrence and live-index integration tests.

#![allow(dead_code)]

use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};
use chrono_tz::Europe::Berlin;
use matrix_calendar_sdk::{
    events::{
        CalendarEvent, CalendarEventEventContent, CalendarInfo, CalendarInfoEventContent,
        EventTime, Frequency, RRule, Recurrence,
    },
    ids,
};
use matrix_sdk::ruma::{
    EventId, RoomId, UserId,
    events::{AnySyncStateEvent, AnySyncTimelineEvent, AnyTimelineEvent},
    serde::Raw,
    user_id,
};
use matrix_sdk_test::{JoinedRoomBuilder, event_factory::EventFactory};
use serde_json::json;

/// The user the mock client is logged in as.
pub const OWN: &str = "@example:localhost";

pub fn own() -> &'static UserId {
    user_id!("@example:localhost")
}

pub fn alice() -> &'static UserId {
    user_id!("@alice:example.org")
}

pub fn local(s: &str) -> NaiveDateTime {
    s.parse().expect("valid local datetime")
}

pub fn utc(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s)
        .expect("valid RFC 3339")
        .with_timezone(&Utc)
}

/// A timed event in Europe/Berlin.
pub fn event(uid: &str, title: &str, organizer: &UserId, start: &str, end: &str) -> CalendarEvent {
    let mut ev = CalendarEvent::new(
        title,
        EventTime::timed(local(start), Berlin).expect("valid start"),
        EventTime::timed(local(end), Berlin).expect("valid end"),
        organizer.to_owned(),
    );
    ev.uid = uid.to_owned();
    ev
}

/// An all-day event on `date` (one day).
pub fn all_day(uid: &str, title: &str, organizer: &UserId, date: &str) -> CalendarEvent {
    let date: NaiveDate = date.parse().expect("valid date");
    let mut ev = CalendarEvent::new(
        title,
        EventTime::all_day(date),
        EventTime::all_day(date.succ_opt().expect("not the last date")),
        organizer.to_owned(),
    );
    ev.uid = uid.to_owned();
    ev
}

/// A weekly series every Wednesday 09:00–09:30 Berlin, starting 2026-07-01.
pub fn weekly_standup(organizer: &UserId) -> CalendarEvent {
    let mut ev = event(
        "standup-uid",
        "Team standup",
        organizer,
        "2026-07-01T09:00:00",
        "2026-07-01T09:30:00",
    );
    ev.recurrence = Some(Recurrence::new(RRule::new(Frequency::Weekly)));
    ev
}

pub fn factory(sender: &UserId) -> EventFactory {
    EventFactory::new().sender(sender)
}

pub fn original(
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

pub fn original_in_room(
    room_id: &RoomId,
    sender: &UserId,
    id: &EventId,
    ts: u64,
    ev: CalendarEvent,
) -> Raw<AnyTimelineEvent> {
    factory(sender)
        .room(room_id)
        .event(CalendarEventEventContent::new(ev))
        .event_id(id)
        .server_ts(ts)
        .into_raw_timeline()
}

pub fn edit(
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

pub fn override_event(
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

pub fn create_state() -> Raw<AnySyncStateEvent> {
    Raw::new(&json!({
        "type": "m.room.create",
        "state_key": "",
        "event_id": "$create",
        "sender": OWN,
        "origin_server_ts": 1,
        "content": {
            "creator": OWN,
            "room_version": "10",
            "type": ids::ROOM_TYPE_CALENDAR_UNSTABLE,
        },
    }))
    .expect("serializable")
    .cast_unchecked()
}

/// A `calendar_info` state event (unstable type) with the given home timezone.
pub fn info_state(timezone: chrono_tz::Tz) -> Raw<AnySyncStateEvent> {
    let content = CalendarInfoEventContent::new(
        "Calendar",
        CalendarInfo::new(None, Some(timezone)).expect("valid info"),
    )
    .expect("valid content");
    Raw::new(&json!({
        "type": ids::CALENDAR_INFO_UNSTABLE,
        "state_key": "",
        "event_id": "$info",
        "sender": OWN,
        "origin_server_ts": 2,
        "content": content,
    }))
    .expect("serializable")
    .cast_unchecked()
}

pub fn calendar_room(room_id: &RoomId) -> JoinedRoomBuilder {
    JoinedRoomBuilder::new(room_id).add_state_event(create_state())
}
