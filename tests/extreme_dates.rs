//! Extreme dates from remote data must be rejected or handled, never panic.

use chrono::{DateTime, NaiveDate, NaiveDateTime, TimeZone, Utc};
use chrono_tz::{America::Los_Angeles, Europe::Berlin, Pacific::Kiritimati, Tz};
use matrix_calendar_sdk::{
    Error,
    availability::{compute_busy, compute_busy_from_occurrences, unavailable_from_windows},
    calendar::CalendarIndex,
    events::{
        AvailabilityWindowEventContent, CalendarEvent, CalendarEventEventContent, EventTime,
        Frequency, RRule, Recurrence, Until, Weekday, Window,
    },
    recurrence::{ExpandOptions, expand, next_occurrence},
};
use matrix_sdk::ruma::{events::AnySyncTimelineEvent, owned_user_id, serde::Raw};
use serde_json::{Value, json};

const EXTREME_DATES: [&str; 6] = [
    "-262143-01-01",
    "+262142-12-30",
    "0000-12-31",
    "10000-01-01",
    "-0001-06-15",
    "+99999-01-01",
];

const EXTREME_LOCALTIMES: [&str; 4] = [
    "-262143-01-01T00:00:00",
    "+262142-12-31T23:59:59",
    "0000-01-01T00:30:00",
    "10000-01-01T00:00:00",
];

fn timezones() -> [Tz; 4] {
    [Tz::UTC, Berlin, Kiritimati, Los_Angeles]
}

/// Values a caller can still build programmatically, bypassing deserialization.
fn extreme_times() -> Vec<EventTime> {
    let mut out = vec![
        EventTime::all_day(NaiveDate::MIN),
        EventTime::all_day(NaiveDate::MAX),
        EventTime::all_day(NaiveDate::from_ymd_opt(1, 1, 1).unwrap()),
        EventTime::all_day(NaiveDate::from_ymd_opt(9999, 12, 31).unwrap()),
    ];
    for tz in timezones() {
        out.push(EventTime::from_utc(DateTime::<Utc>::MIN_UTC, tz));
        out.push(EventTime::from_utc(DateTime::<Utc>::MAX_UTC, tz));
        out.push(EventTime::Timed {
            utc: DateTime::<Utc>::MAX_UTC,
            localtime: NaiveDateTime::MIN,
            timezone: tz,
        });
        out.push(EventTime::Timed {
            utc: DateTime::<Utc>::MIN_UTC,
            localtime: NaiveDateTime::MAX,
            timezone: tz,
        });
    }
    out
}

fn event_with(start: EventTime, end: EventTime) -> CalendarEvent {
    let mut ev = CalendarEvent::new("Extreme", start, end, owned_user_id!("@alice:example.org"));
    ev.uid = "extreme".into();
    ev
}

fn full_range() -> ExpandOptions {
    ExpandOptions::new(DateTime::<Utc>::MIN_UTC, DateTime::<Utc>::MAX_UTC).with_limit(Some(50))
}

#[test]
fn deserialization_rejects_out_of_range_years() {
    for date in EXTREME_DATES {
        let err = serde_json::from_value::<EventTime>(json!({ "date": date }));
        assert!(err.is_err(), "{date} should be rejected");
    }
    for localtime in EXTREME_LOCALTIMES {
        let err = serde_json::from_value::<EventTime>(json!({
            "utc": "2026-07-01T07:00:00Z",
            "localtime": localtime,
            "timezone": "Europe/Berlin"
        }));
        assert!(err.is_err(), "{localtime} should be rejected");
    }
    for utc in ["0000-01-01T00:00:00Z", "0001-01-01T00:30:00+01:00"] {
        let err = serde_json::from_value::<EventTime>(json!({
            "utc": utc,
            "localtime": "0001-01-01T00:00:00",
            "timezone": "UTC"
        }));
        assert!(err.is_err(), "{utc} should be rejected");
    }
    // The boundaries themselves are fine.
    for date in ["0001-01-01", "9999-12-31"] {
        let t: EventTime = serde_json::from_value(json!({ "date": date })).unwrap();
        t.validate().unwrap();
    }

    // Through a whole event, an UNTIL and an exdate.
    let mut ev = json!({
        "uid": "u", "title": "T",
        "start": { "date": "2026-01-01" }, "end": { "date": "+262142-12-30" },
        "all_day": true, "organizer": "@alice:example.org"
    });
    assert!(serde_json::from_value::<CalendarEvent>(ev.clone()).is_err());
    ev["end"] = json!({ "date": "2026-01-02" });
    ev["recurrence"] = json!({ "rrule": { "freq": "DAILY" }, "exdates": ["0000-01-01T00:00:00Z"] });
    assert!(serde_json::from_value::<CalendarEvent>(ev).is_err());
}

#[test]
fn constructors_reject_out_of_range_years() {
    for local in [NaiveDateTime::MIN, NaiveDateTime::MAX] {
        for tz in timezones() {
            assert!(matches!(
                EventTime::timed(local, tz),
                Err(Error::InvalidDateTime(_))
            ));
        }
    }
    // 0001-01-01 00:30 in Tokyo (local mean time, +09:18:59) is still in year 0 in UTC.
    assert!(
        EventTime::timed(
            "0001-01-01T00:30:00".parse().unwrap(),
            chrono_tz::Asia::Tokyo
        )
        .is_err()
    );
    assert!(EventTime::try_all_day(NaiveDate::MIN).is_err());
    assert!(EventTime::try_all_day(NaiveDate::MAX).is_err());
    for t in extreme_times().into_iter().filter(|t| {
        !matches!(t, EventTime::AllDay { date } if date.to_string().starts_with("0001")
            || date.to_string().starts_with("9999"))
    }) {
        assert!(t.validate().is_err(), "{t:?} should not validate");
    }
}

#[test]
fn to_utc_never_panics() {
    for t in extreme_times() {
        for tz in timezones() {
            let _ = t.to_utc(tz);
            let _ = t.to_string();
            let _ = t.validate();
        }
    }
}

#[test]
fn compute_busy_never_panics() {
    let times = extreme_times();
    let mut events = Vec::new();
    for start in &times {
        for end in &times {
            events.push(event_with(*start, *end));
        }
    }
    for tz in timezones() {
        for titles in [false, true] {
            let _ = compute_busy(
                &events,
                DateTime::<Utc>::MIN_UTC,
                DateTime::<Utc>::MAX_UTC,
                tz,
                titles,
            );
        }
    }

    // A recurring master is skipped rather than counted as one instance.
    let mut series = event_with(
        EventTime::all_day(NaiveDate::MIN),
        EventTime::all_day(NaiveDate::MAX),
    );
    series.recurrence = Some(Recurrence::new(RRule::new(Frequency::Daily)));
    assert!(
        compute_busy(
            [&series],
            DateTime::<Utc>::MIN_UTC,
            DateTime::<Utc>::MAX_UTC,
            Tz::UTC,
            false
        )
        .is_empty()
    );
}

#[test]
fn expand_never_panics() {
    let times = extreme_times();
    for start in &times {
        for end in &times {
            // Non-recurring: one occurrence or none, no arithmetic.
            let single = event_with(*start, *end);
            for tz in timezones() {
                let opts = full_range().with_fallback_tz(tz);
                let _ = expand(&single, &[], &opts);
                let _ = next_occurrence(&single, &[], DateTime::<Utc>::MIN_UTC, tz);
            }

            // Recurring masters outside the supported years are rejected.
            let mut series = single.clone();
            series.recurrence = Some(Recurrence::new(RRule::new(Frequency::Daily)));
            let result = expand(&series, &[], &full_range());
            if start.check_year_range().is_err() || end.check_year_range().is_err() {
                assert!(
                    matches!(
                        result,
                        Err(Error::InvalidDateTime(_) | Error::InconsistentDateTime(_))
                    ),
                    "{start:?}–{end:?}: {result:?}"
                );
            }
            let _ = next_occurrence(&series, &[], DateTime::<Utc>::MIN_UTC, Tz::UTC);
        }
    }
}

#[test]
fn expand_near_the_supported_limits() {
    let opts = full_range();

    // A yearly series at the very end of year 9999 runs out of years without panicking.
    for (start, end) in [
        (
            EventTime::all_day(NaiveDate::from_ymd_opt(9999, 12, 31).unwrap()),
            EventTime::all_day(
                NaiveDate::from_ymd_opt(9999, 12, 31)
                    .unwrap()
                    .succ_opt()
                    .unwrap(),
            ),
        ),
        (
            EventTime::timed("9999-12-31T23:00:00".parse().unwrap(), Los_Angeles).unwrap_or(
                EventTime::from_utc(
                    Utc.with_ymd_and_hms(9999, 12, 31, 0, 0, 0).unwrap(),
                    Tz::UTC,
                ),
            ),
            EventTime::from_utc(
                Utc.with_ymd_and_hms(9999, 12, 31, 23, 0, 0).unwrap(),
                Tz::UTC,
            ),
        ),
    ] {
        let mut ev = event_with(start, end);
        for freq in [Frequency::Yearly, Frequency::Daily, Frequency::Secondly] {
            ev.recurrence = Some(Recurrence::new(RRule::new(freq)));
            let _ = expand(&ev, &[], &opts);
            let _ = next_occurrence(&ev, &[], DateTime::<Utc>::MIN_UTC, Kiritimati);
        }
    }

    // A series at the start of year 1 with exdates, rdates and overrides at chrono's limits.
    let mut ev = event_with(
        EventTime::all_day(NaiveDate::from_ymd_opt(1, 1, 1).unwrap()),
        EventTime::all_day(NaiveDate::from_ymd_opt(1, 1, 2).unwrap()),
    );
    let mut recurrence = Recurrence::new(RRule {
        count: Some(3),
        ..RRule::new(Frequency::Daily)
    });
    recurrence.exdates = vec![DateTime::<Utc>::MIN_UTC, DateTime::<Utc>::MAX_UTC];
    recurrence.rdates = vec![DateTime::<Utc>::MIN_UTC, DateTime::<Utc>::MAX_UTC];
    ev.recurrence = Some(recurrence);
    let mut extreme_override = ev.clone();
    extreme_override.recurrence = None;
    extreme_override.recurrence_id = Some(DateTime::<Utc>::MIN_UTC);
    for tz in timezones() {
        let occ = expand(&ev, [&extreme_override], &opts.with_fallback_tz(tz)).unwrap();
        assert!(occ.len() >= 3, "{occ:?}");
    }
    // The same for a timed series in each timezone.
    for tz in timezones() {
        let mut timed = ev.clone();
        timed.start = EventTime::timed("0001-01-02T10:00:00".parse().unwrap(), tz).unwrap();
        timed.end = EventTime::timed("0001-01-02T11:00:00".parse().unwrap(), tz).unwrap();
        timed.all_day = false;
        let mut timed_override = extreme_override.clone();
        timed_override.start = timed.start;
        timed_override.end = timed.end;
        timed_override.all_day = false;
        let occ = expand(&timed, [&timed_override], &opts.with_fallback_tz(tz)).unwrap();
        assert!(occ.len() >= 3, "{occ:?}");
        let _ = next_occurrence(&timed, [&timed_override], DateTime::<Utc>::MIN_UTC, tz);
    }

    // UNTIL outside the supported years is rejected.
    for until in [
        Until::Date(NaiveDate::MAX),
        Until::Date(NaiveDate::MIN),
        Until::DateTime(DateTime::<Utc>::MAX_UTC),
    ] {
        let mut ev = event_with(
            EventTime::all_day(NaiveDate::from_ymd_opt(2026, 1, 1).unwrap()),
            EventTime::all_day(NaiveDate::from_ymd_opt(2026, 1, 2).unwrap()),
        );
        ev.recurrence = Some(Recurrence::new(RRule {
            until: Some(until),
            ..RRule::new(Frequency::Daily)
        }));
        assert!(matches!(
            expand(&ev, &[], &opts),
            Err(Error::InvalidDateTime(_))
        ));
    }
}

#[test]
fn availability_windows_at_chrono_limits() {
    let windows = AvailabilityWindowEventContent::new(
        timezones()
            .into_iter()
            .map(|timezone| Window {
                days: vec![Weekday::Monday, Weekday::Saturday, Weekday::Sunday],
                start_time: "22:00:00".parse().unwrap(),
                end_time: "02:00:00".parse().unwrap(),
                timezone,
            })
            .collect(),
    );
    let ten_days = chrono::TimeDelta::days(10);
    let min = DateTime::<Utc>::MIN_UTC;
    let max = DateTime::<Utc>::MAX_UTC;
    let _ = unavailable_from_windows(&windows, min, min + ten_days);
    let _ = unavailable_from_windows(&windows, max - ten_days, max);
}

fn raw_event(id: &str, content: Value) -> Raw<AnySyncTimelineEvent> {
    Raw::new(&json!({
        "type": "org.matrix.msc4496.calendar_event",
        "event_id": id,
        "sender": "@alice:example.org",
        "origin_server_ts": 1,
        "content": content,
    }))
    .unwrap()
    .cast_unchecked()
}

#[test]
fn index_skips_extreme_events_and_handles_boundaries() {
    let mut index = CalendarIndex::new();

    // Events with out-of-range dates from the room are skipped, not indexed.
    for (i, date) in EXTREME_DATES.iter().enumerate() {
        let applied = index
            .apply_raw_sync(&raw_event(
                &format!("$bad{i}"),
                json!({
                    "org.matrix.msc4496.calendar_event": {
                        "uid": format!("bad{i}"), "title": "Bad",
                        "start": { "date": date }, "end": { "date": date },
                        "all_day": true
                    }
                }),
            ))
            .unwrap();
        assert!(!applied);
    }
    assert!(index.is_empty());

    // Events at the boundaries of the supported years work with any range.
    let first = NaiveDate::from_ymd_opt(1, 1, 1).unwrap();
    let last = NaiveDate::from_ymd_opt(9999, 12, 31).unwrap();
    let mut early = event_with(
        EventTime::all_day(first),
        EventTime::all_day(first.succ_opt().unwrap()),
    );
    early.uid = "early".into();
    let mut late = event_with(
        EventTime::all_day(last.pred_opt().unwrap()),
        EventTime::all_day(last),
    );
    late.uid = "late".into();
    late.recurrence = Some(Recurrence::new(RRule::new(Frequency::Daily)));
    for (id, ev) in [("$early", early), ("$late", late)] {
        let content = serde_json::to_value(CalendarEventEventContent::new(ev)).unwrap();
        assert!(index.apply_raw_sync(&raw_event(id, content)).unwrap());
    }

    let (min, max) = (DateTime::<Utc>::MIN_UTC, DateTime::<Utc>::MAX_UTC);
    assert_eq!(index.events_in_range(min, max).len(), 2);
    assert_eq!(index.events_in_range(max, min).len(), 1); // only the recurring one
    for tz in timezones() {
        let occ = index.occurrences(min, max, tz).unwrap();
        assert!(!occ.is_empty());
        let _ = index.busy(min, max, tz, false).unwrap();
        let _ = index.next_occurrence(min, tz).unwrap();
        let busy =
            compute_busy_from_occurrences(occ.iter().map(|o| &o.occurrence), min, max, tz, true);
        assert!(!busy.is_empty());
    }
}
