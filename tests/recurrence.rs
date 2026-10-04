//! Recurrence expansion through the public API.

use chrono::{DateTime, Datelike, NaiveDate, Timelike, Utc};
use chrono_tz::{America::New_York, Europe::Berlin, Tz};
use matrix_calendar_sdk::{
    Error,
    events::{
        ByDay, CalendarEvent, EventTime, Frequency, RRule, Recurrence, Status, Until, Weekday,
        datetime::parse_utc,
    },
    recurrence::{ExpandOptions, Occurrence, expand, next_occurrence, to_rrule_set},
};
use matrix_sdk::ruma::owned_user_id;

fn utc(s: &str) -> DateTime<Utc> {
    parse_utc(s).unwrap()
}

fn date(s: &str) -> NaiveDate {
    s.parse().unwrap()
}

fn timed(local: &str, tz: Tz) -> EventTime {
    EventTime::timed(local.parse().unwrap(), tz).unwrap()
}

fn event(title: &str, start: EventTime, end: EventTime, rule: Option<RRule>) -> CalendarEvent {
    let mut ev = CalendarEvent::new(title, start, end, owned_user_id!("@alice:example.com"));
    ev.uid = "550e8400-e29b-41d4-a716-446655440000".into();
    ev.recurrence = rule.map(Recurrence::new);
    ev
}

fn range(start: &str, end: &str) -> ExpandOptions {
    ExpandOptions::new(utc(start), utc(end))
}

fn starts(occ: &[Occurrence]) -> Vec<DateTime<Utc>> {
    occ.iter().map(|o| o.start_utc(Tz::UTC)).collect()
}

fn localtime(t: &EventTime) -> chrono::NaiveDateTime {
    match t {
        EventTime::Timed { localtime, .. } => *localtime,
        EventTime::AllDay { .. } => panic!("expected a timed value"),
    }
}

#[test]
fn autumn_overlap_preserves_stored_start_and_exact_duration() {
    // Both occurrences of 02:30 local are valid; UTC disambiguates them. Check both
    // an ambiguous start and an ambiguous end (including identical wall-clock times).
    for (start, end) in [
        ("2026-10-25T01:30:00Z", "2026-10-25T02:30:00Z"),
        ("2026-10-25T00:30:00Z", "2026-10-25T01:30:00Z"),
        ("2026-10-24T23:30:00Z", "2026-10-25T01:30:00Z"),
    ] {
        for count in [1, 3] {
            let ev = event(
                "Across the autumn overlap",
                EventTime::from_utc(utc(start), Berlin),
                EventTime::from_utc(utc(end), Berlin),
                Some(RRule {
                    count: Some(count),
                    ..RRule::new(Frequency::Daily)
                }),
            );
            ev.validate().unwrap();
            let opts = range("2026-10-24T00:00:00Z", "2026-10-29T00:00:00Z");
            let occurrences = expand(&ev, &[], &opts).unwrap();
            assert_eq!(occurrences.len(), count as usize, "{start} to {end}");
            assert_eq!(occurrences[0].start, ev.start);
            assert_eq!(occurrences[0].end, ev.end);
            assert_eq!(occurrences[0].recurrence_id, Some(utc(start)));
            let duration = utc(end) - utc(start);
            for occurrence in &occurrences {
                assert_eq!(
                    occurrence.end_utc(Berlin) - occurrence.start_utc(Berlin),
                    duration
                );
                assert_eq!(
                    localtime(&occurrence.start).time(),
                    localtime(&ev.start).time()
                );
            }
            let next =
                next_occurrence(&ev, &[], utc(start) - chrono::TimeDelta::seconds(1), Berlin)
                    .unwrap()
                    .unwrap();
            assert_eq!(next.start, ev.start);
            assert_eq!(next.end, ev.end);
            assert_eq!(
                to_rrule_set(&ev)
                    .unwrap()
                    .get_dt_start()
                    .with_timezone(&Utc),
                utc(start)
            );
        }
    }
}

#[test]
fn later_autumn_start_uses_stored_instant_for_exdates_and_overrides() {
    let mut ev = event(
        "Later 02:30",
        EventTime::from_utc(utc("2026-10-25T01:30:00Z"), Berlin),
        EventTime::from_utc(utc("2026-10-25T02:30:00Z"), Berlin),
        Some(RRule {
            count: Some(2),
            ..RRule::new(Frequency::Daily)
        }),
    );
    let opts = range("2026-10-25T00:00:00Z", "2026-10-28T00:00:00Z");
    let mut moved = ev.clone();
    moved.recurrence = None;
    moved.recurrence_id = Some(utc("2026-10-25T01:30:00Z"));
    moved.start = EventTime::from_utc(utc("2026-10-25T03:00:00Z"), Berlin);
    moved.end = EventTime::from_utc(utc("2026-10-25T04:00:00Z"), Berlin);
    let occurrences = expand(&ev, [&moved], &opts).unwrap();
    assert_eq!(occurrences.len(), 2);
    assert!(occurrences[0].is_override);
    assert_eq!(occurrences[0].start, moved.start);

    ev.recurrence
        .as_mut()
        .unwrap()
        .exdates
        .push(utc("2026-10-25T01:30:00Z"));
    let occurrences = expand(&ev, &[], &opts).unwrap();
    assert_eq!(occurrences.len(), 1);
    assert_eq!(
        occurrences[0].start_utc(Berlin),
        utc("2026-10-26T01:30:00Z")
    );
}

/// The MSC example: weekly on Monday, 09:00-09:30 Europe/Berlin, from Wednesday 2026-07-01
/// until 2026-12-31. The Wednesday start is the first instance (RFC 8984 §4.3.3).
fn msc_standup() -> CalendarEvent {
    let mut ev = event(
        "Team standup",
        timed("2026-07-01T09:00:00", Berlin),
        timed("2026-07-01T09:30:00", Berlin),
        Some(RRule {
            byday: Some(vec![ByDay::every(Weekday::Monday)]),
            until: Some(Until::Date(date("2026-12-31"))),
            ..RRule::new(Frequency::Weekly)
        }),
    );
    if let Some(r) = &mut ev.recurrence {
        r.exdates = vec![
            // From the MSC: a Wednesday, matches no Monday instance.
            utc("2026-08-05T07:00:00Z"),
            // Monday 2026-08-10 09:00 CEST: removes that instance.
            utc("2026-08-10T07:00:00Z"),
        ];
    }
    ev
}

#[test]
fn msc_example_weekly_across_dst() {
    let ev = msc_standup();
    let occ = expand(
        &ev,
        &[],
        &range("2026-01-01T00:00:00Z", "2027-01-01T00:00:00Z"),
    )
    .unwrap();

    // The start (Wednesday 2026-07-01) plus 26 Mondays from 2026-07-06 to 2026-12-28, minus
    // the excluded 2026-08-10.
    assert_eq!(occ.len(), 26);
    assert_eq!(occ[0].start_utc(Tz::UTC), utc("2026-07-01T07:00:00Z"));
    assert_eq!(occ[0].start, ev.start);
    assert_eq!(occ[0].end, ev.end);
    assert_eq!(occ[1].start_utc(Tz::UTC), utc("2026-07-06T07:00:00Z"));
    assert_eq!(occ[25].start_utc(Tz::UTC), utc("2026-12-28T08:00:00Z"));
    assert!(!starts(&occ).contains(&utc("2026-08-10T07:00:00Z")));
    assert!(starts(&occ).contains(&utc("2026-08-17T07:00:00Z")));

    // DST ends on 2026-10-25: 09:00 local stays 09:00, UTC flips from 07:00Z to 08:00Z.
    assert!(starts(&occ).contains(&utc("2026-10-19T07:00:00Z")));
    assert!(starts(&occ).contains(&utc("2026-10-26T08:00:00Z")));
    for o in &occ {
        assert_eq!(o.uid, ev.uid);
        assert!(!o.is_override);
        assert_eq!(o.event, ev);
        assert_eq!(o.recurrence_id, Some(o.start_utc(Tz::UTC)));
        assert_eq!(o.start.timezone(), Some(Berlin));
        assert_eq!(localtime(&o.start).hour(), 9);
        assert_eq!(
            localtime(&o.end) - localtime(&o.start),
            chrono::TimeDelta::minutes(30)
        );
        o.start.validate().unwrap();
        o.end.validate().unwrap();
    }

    for o in &occ[1..] {
        assert_eq!(localtime(&o.start).date().weekday(), chrono::Weekday::Mon);
    }

    // Ordering.
    let s = starts(&occ);
    assert!(s.windows(2).all(|w| w[0] < w[1]));

    // The conversion to the rrule crate expands in the named timezone.
    let set = to_rrule_set(&ev).unwrap();
    assert!(
        set.to_string()
            .contains("DTSTART;TZID=Europe/Berlin:20260701T090000")
    );
}

#[test]
fn until_date_is_inclusive_for_timed_events() {
    let mut ev = msc_standup();
    if let Some(r) = &mut ev.recurrence {
        r.rrule.until = Some(Until::Date(date("2026-12-28")));
    }
    let occ = expand(
        &ev,
        &[],
        &range("2026-12-01T00:00:00Z", "2027-02-01T00:00:00Z"),
    )
    .unwrap();
    assert_eq!(
        occ.last().unwrap().start_utc(Tz::UTC),
        utc("2026-12-28T08:00:00Z")
    );

    // A date-time UNTIL is an exact inclusive bound.
    if let Some(r) = &mut ev.recurrence {
        r.rrule.until = Some(Until::DateTime(utc("2026-12-28T07:59:59Z")));
    }
    let occ = expand(
        &ev,
        &[],
        &range("2026-12-01T00:00:00Z", "2027-02-01T00:00:00Z"),
    )
    .unwrap();
    assert_eq!(
        occ.last().unwrap().start_utc(Tz::UTC),
        utc("2026-12-21T08:00:00Z")
    );
}

#[test]
fn daily_with_count_in_limited_range() {
    let ev = event(
        "Daily",
        timed("2026-03-01T10:00:00", Berlin),
        timed("2026-03-01T11:00:00", Berlin),
        Some(RRule {
            count: Some(5),
            ..RRule::new(Frequency::Daily)
        }),
    );

    let all = expand(
        &ev,
        &[],
        &range("2026-01-01T00:00:00Z", "2027-01-01T00:00:00Z"),
    )
    .unwrap();
    assert_eq!(all.len(), 5);
    assert_eq!(all[4].start_utc(Tz::UTC), utc("2026-03-05T09:00:00Z"));

    // The 03-02 instance (09:00Z-10:00Z) straddles range_start; 03-05 starts at range_end.
    let some = expand(
        &ev,
        &[],
        &range("2026-03-02T09:30:00Z", "2026-03-05T09:00:00Z"),
    )
    .unwrap();
    assert_eq!(
        starts(&some),
        [
            utc("2026-03-02T09:00:00Z"),
            utc("2026-03-03T09:00:00Z"),
            utc("2026-03-04T09:00:00Z"),
        ]
    );

    // An instance ending exactly at range_start does not intersect.
    let none = expand(
        &ev,
        &[],
        &range("2026-03-05T10:00:00Z", "2026-03-06T00:00:00Z"),
    )
    .unwrap();
    assert!(none.is_empty());
}

#[test]
fn rdates_are_added_and_deduplicated() {
    let mut ev = event(
        "Daily",
        timed("2026-03-01T10:00:00", Berlin),
        timed("2026-03-01T11:00:00", Berlin),
        Some(RRule {
            count: Some(2),
            ..RRule::new(Frequency::Daily)
        }),
    );
    if let Some(r) = &mut ev.recurrence {
        r.rdates = vec![utc("2026-03-10T15:00:00Z"), utc("2026-03-02T09:00:00Z")];
    }
    let occ = expand(
        &ev,
        &[],
        &range("2026-01-01T00:00:00Z", "2027-01-01T00:00:00Z"),
    )
    .unwrap();
    assert_eq!(
        starts(&occ),
        [
            utc("2026-03-01T09:00:00Z"),
            utc("2026-03-02T09:00:00Z"),
            utc("2026-03-10T15:00:00Z"),
        ]
    );
    assert_eq!(occ[2].end_utc(Tz::UTC), utc("2026-03-10T16:00:00Z"));
}

#[test]
fn overrides_replace_and_cancel_instances() {
    let master = msc_standup();
    let override_of = |rid: &str, start: &str, end: &str, title: &str| {
        let mut o = master.clone();
        o.recurrence = None;
        o.recurrence_id = Some(utc(rid));
        o.title = title.into();
        o.start = timed(start, Berlin);
        o.end = timed(end, Berlin);
        o
    };

    let moved = override_of(
        "2026-07-13T07:00:00Z",
        "2026-07-14T14:00:00",
        "2026-07-14T15:00:00",
        "Moved standup",
    );
    let mut cancelled = override_of(
        "2026-07-20T07:00:00Z",
        "2026-07-20T09:00:00",
        "2026-07-20T09:30:00",
        "Team standup",
    );
    cancelled.status = Status::Cancelled;
    // Matches no generated instance (a Wednesday), but is still emitted.
    let orphan = override_of(
        "2026-07-15T07:00:00Z",
        "2026-07-15T12:00:00",
        "2026-07-15T13:00:00",
        "Orphan",
    );
    // Moves the 07-27 instance into August.
    let into_august = override_of(
        "2026-07-27T07:00:00Z",
        "2026-08-01T10:00:00",
        "2026-08-01T11:00:00",
        "Moved to August",
    );
    let overrides = [
        moved.clone(),
        cancelled,
        orphan.clone(),
        into_august.clone(),
    ];

    let july = expand(
        &master,
        &overrides,
        &range("2026-07-01T00:00:00Z", "2026-08-01T00:00:00Z"),
    )
    .unwrap();
    assert_eq!(
        starts(&july),
        [
            utc("2026-07-01T07:00:00Z"),
            utc("2026-07-06T07:00:00Z"),
            utc("2026-07-14T12:00:00Z"),
            utc("2026-07-15T10:00:00Z"),
        ]
    );
    assert!(!july[0].is_override);
    assert!(!july[1].is_override);
    assert!(july[2].is_override);
    assert_eq!(july[2].recurrence_id, Some(utc("2026-07-13T07:00:00Z")));
    assert_eq!(july[2].event.title, "Moved standup");
    assert_eq!(july[2].event, moved);
    assert_eq!(july[2].end, moved.end);
    assert!(july[3].is_override);
    assert_eq!(july[3].event, orphan);

    let august = expand(
        &master,
        &overrides,
        &range("2026-08-01T00:00:00Z", "2026-08-08T00:00:00Z"),
    )
    .unwrap();
    assert_eq!(
        starts(&august),
        [utc("2026-08-01T08:00:00Z"), utc("2026-08-03T07:00:00Z")]
    );
    assert_eq!(august[0].event, into_august);
    assert_eq!(august[0].recurrence_id, Some(utc("2026-07-27T07:00:00Z")));

    // A mismatched uid is an error.
    let mut foreign = moved.clone();
    foreign.uid = "someone-else".into();
    assert!(matches!(
        expand(
            &master,
            &[foreign],
            &range("2026-07-01T00:00:00Z", "2026-08-01T00:00:00Z")
        ),
        Err(Error::InvalidIdentifier(_))
    ));
    // So is an override without recurrence_id.
    let mut no_rid = moved;
    no_rid.recurrence_id = None;
    assert!(matches!(
        expand(
            &master,
            &[no_rid],
            &range("2026-07-01T00:00:00Z", "2026-08-01T00:00:00Z")
        ),
        Err(Error::InvalidRecurrence(_))
    ));
}

#[test]
fn all_day_weekly_with_exclusive_end() {
    let mut ev = event(
        "Holiday",
        EventTime::all_day(date("2026-12-25")),
        EventTime::all_day(date("2026-12-26")),
        Some(RRule {
            count: Some(3),
            ..RRule::new(Frequency::Weekly)
        }),
    );

    let occ = expand(
        &ev,
        &[],
        &range("2026-12-01T00:00:00Z", "2027-02-01T00:00:00Z"),
    )
    .unwrap();
    assert_eq!(occ.len(), 3);
    let dates: Vec<_> = occ.iter().map(|o| (o.start, o.end)).collect();
    assert_eq!(
        dates,
        [
            (
                EventTime::all_day(date("2026-12-25")),
                EventTime::all_day(date("2026-12-26"))
            ),
            (
                EventTime::all_day(date("2027-01-01")),
                EventTime::all_day(date("2027-01-02"))
            ),
            (
                EventTime::all_day(date("2027-01-08")),
                EventTime::all_day(date("2027-01-09"))
            ),
        ]
    );
    assert_eq!(occ[1].recurrence_id, Some(utc("2027-01-01T00:00:00Z")));

    // End is exclusive: neither 12-25 (ends at range_start) nor 01-01 (starts at range_end).
    let none = expand(
        &ev,
        &[],
        &range("2026-12-26T00:00:00Z", "2027-01-01T00:00:00Z"),
    )
    .unwrap();
    assert!(none.is_empty());
    let one = expand(
        &ev,
        &[],
        &range("2027-01-01T00:00:00Z", "2027-01-01T00:00:01Z"),
    )
    .unwrap();
    assert_eq!(one.len(), 1);
    assert_eq!(one[0].start, EventTime::all_day(date("2027-01-01")));

    // With a fallback timezone, dates are placed at local midnight.
    let berlin = range("2026-12-31T23:00:00Z", "2027-01-01T00:00:00Z").with_fallback_tz(Berlin);
    let occ = expand(&ev, &[], &berlin).unwrap();
    assert_eq!(occ.len(), 1);
    assert_eq!(occ[0].start_utc(Berlin), utc("2026-12-31T23:00:00Z"));

    // An exdate at local midnight (as a Berlin client would write it) removes 01-01.
    if let Some(r) = &mut ev.recurrence {
        r.exdates = vec![utc("2026-12-31T23:00:00Z")];
    }
    let opts = range("2026-12-01T00:00:00Z", "2027-02-01T00:00:00Z").with_fallback_tz(Berlin);
    let occ = expand(&ev, &[], &opts).unwrap();
    assert_eq!(
        occ.iter().map(|o| o.start).collect::<Vec<_>>(),
        [
            EventTime::all_day(date("2026-12-25")),
            EventTime::all_day(date("2027-01-08"))
        ]
    );

    // An exdate at UTC midnight works too, as does an override keyed by the date.
    if let Some(r) = &mut ev.recurrence {
        r.exdates = vec![utc("2027-01-08T00:00:00Z")];
    }
    let mut cancelled = ev.clone();
    cancelled.recurrence = None;
    cancelled.recurrence_id = Some(utc("2026-12-24T23:00:00Z"));
    cancelled.status = Status::Cancelled;
    let occ = expand(&ev, &[cancelled], &opts).unwrap();
    assert_eq!(
        occ.iter().map(|o| o.start).collect::<Vec<_>>(),
        [EventTime::all_day(date("2027-01-01"))]
    );
}

#[test]
fn multi_day_all_day_series() {
    let ev = event(
        "Conference",
        EventTime::all_day(date("2026-06-01")),
        EventTime::all_day(date("2026-06-04")),
        Some(RRule {
            count: Some(2),
            ..RRule::new(Frequency::Yearly)
        }),
    );
    let occ = expand(
        &ev,
        &[],
        &range("2027-06-03T12:00:00Z", "2027-06-03T13:00:00Z"),
    )
    .unwrap();
    assert_eq!(occ.len(), 1);
    assert_eq!(occ[0].start, EventTime::all_day(date("2027-06-01")));
    assert_eq!(occ[0].end, EventTime::all_day(date("2027-06-04")));
}

#[test]
fn monthly_last_friday() {
    let ev = event(
        "Retro",
        timed("2026-01-30T18:00:00", New_York),
        timed("2026-01-30T19:00:00", New_York),
        Some(RRule {
            byday: Some(vec![ByDay::nth(-1, Weekday::Friday)]),
            count: Some(4),
            ..RRule::new(Frequency::Monthly)
        }),
    );
    let occ = expand(
        &ev,
        &[],
        &range("2026-01-01T00:00:00Z", "2027-01-01T00:00:00Z"),
    )
    .unwrap();
    // New York switches to DST on 2026-03-08.
    assert_eq!(
        starts(&occ),
        [
            utc("2026-01-30T23:00:00Z"),
            utc("2026-02-27T23:00:00Z"),
            utc("2026-03-27T22:00:00Z"),
            utc("2026-04-24T22:00:00Z"),
        ]
    );
}

#[test]
fn yearly_by_month_and_day() {
    let ev = event(
        "Leap day",
        timed("2024-02-29T12:00:00", Berlin),
        timed("2024-02-29T13:00:00", Berlin),
        Some(RRule {
            bymonth: Some(vec![2]),
            bymonthday: Some(vec![29]),
            count: Some(3),
            ..RRule::new(Frequency::Yearly)
        }),
    );
    let occ = expand(
        &ev,
        &[],
        &range("2020-01-01T00:00:00Z", "2040-01-01T00:00:00Z"),
    )
    .unwrap();
    assert_eq!(
        starts(&occ),
        [
            utc("2024-02-29T11:00:00Z"),
            utc("2028-02-29T11:00:00Z"),
            utc("2032-02-29T11:00:00Z"),
        ]
    );
}

#[test]
fn monthly_last_weekday_by_setpos() {
    let ev = event(
        "Month end",
        timed("2026-01-30T16:00:00", Berlin),
        timed("2026-01-30T17:00:00", Berlin),
        Some(RRule {
            byday: Some(
                [
                    Weekday::Monday,
                    Weekday::Tuesday,
                    Weekday::Wednesday,
                    Weekday::Thursday,
                    Weekday::Friday,
                ]
                .into_iter()
                .map(ByDay::every)
                .collect(),
            ),
            bysetpos: Some(vec![-1]),
            count: Some(3),
            ..RRule::new(Frequency::Monthly)
        }),
    );
    let occ = expand(
        &ev,
        &[],
        &range("2026-01-01T00:00:00Z", "2027-01-01T00:00:00Z"),
    )
    .unwrap();
    let dates: Vec<_> = occ.iter().map(|o| o.start.local_date()).collect();
    assert_eq!(
        dates,
        [date("2026-01-30"), date("2026-02-27"), date("2026-03-31")]
    );
    // March 31 is after the DST change.
    assert_eq!(occ[2].start_utc(Tz::UTC), utc("2026-03-31T14:00:00Z"));
}

#[test]
fn non_recurring_event_in_and_out_of_range() {
    let ev = event(
        "One-off",
        timed("2026-07-01T09:00:00", Berlin),
        timed("2026-07-01T10:00:00", Berlin),
        None,
    );
    let inside = expand(
        &ev,
        &[],
        &range("2026-07-01T07:30:00Z", "2026-07-01T07:31:00Z"),
    )
    .unwrap();
    assert_eq!(inside.len(), 1);
    assert_eq!(inside[0].recurrence_id, None);
    assert_eq!(inside[0].start, ev.start);
    assert_eq!(inside[0].end, ev.end);
    assert!(!inside[0].is_override);

    let before = expand(
        &ev,
        &[],
        &range("2026-06-01T00:00:00Z", "2026-07-01T07:00:00Z"),
    )
    .unwrap();
    assert!(before.is_empty());
    let after = expand(
        &ev,
        &[],
        &range("2026-07-01T08:00:00Z", "2026-08-01T00:00:00Z"),
    )
    .unwrap();
    assert!(after.is_empty());

    // Zero-duration events count when they start inside the range.
    let mut point = ev.clone();
    point.end = point.start;
    let at_start = expand(
        &point,
        &[],
        &range("2026-07-01T07:00:00Z", "2026-07-01T08:00:00Z"),
    );
    assert_eq!(at_start.unwrap().len(), 1);
    let at_end = expand(
        &point,
        &[],
        &range("2026-07-01T06:00:00Z", "2026-07-01T07:00:00Z"),
    );
    assert!(at_end.unwrap().is_empty());
}

#[test]
fn next_occurrence_after_instant() {
    let master = msc_standup();
    let next = |after: &str, overrides: &[CalendarEvent]| {
        next_occurrence(&master, overrides, utc(after), Tz::UTC).unwrap()
    };

    // The series start is the first instance, even though it is a Wednesday.
    assert_eq!(
        next("2026-01-01T00:00:00Z", &[])
            .unwrap()
            .start_utc(Tz::UTC),
        utc("2026-07-01T07:00:00Z")
    );
    assert_eq!(
        next("2026-07-01T07:00:00Z", &[])
            .unwrap()
            .start_utc(Tz::UTC),
        utc("2026-07-06T07:00:00Z")
    );
    // Strictly after: an instance starting exactly at `after` is skipped.
    assert_eq!(
        next("2026-10-19T07:00:00Z", &[])
            .unwrap()
            .start_utc(Tz::UTC),
        utc("2026-10-26T08:00:00Z")
    );
    // Exdates are skipped.
    assert_eq!(
        next("2026-08-04T00:00:00Z", &[])
            .unwrap()
            .start_utc(Tz::UTC),
        utc("2026-08-17T07:00:00Z")
    );
    // Past UNTIL there is nothing.
    assert!(next("2026-12-28T08:00:00Z", &[]).is_none());

    // Overrides: a cancelled one is skipped, a moved one is returned in its new place.
    let mut cancelled = master.clone();
    cancelled.recurrence = None;
    cancelled.recurrence_id = Some(utc("2026-10-26T08:00:00Z"));
    cancelled.status = Status::Cancelled;
    let mut moved = master.clone();
    moved.recurrence = None;
    moved.recurrence_id = Some(utc("2026-11-02T08:00:00Z"));
    moved.start = timed("2026-10-30T09:00:00", Berlin);
    moved.end = timed("2026-10-30T10:00:00", Berlin);
    let overrides = [cancelled, moved.clone()];

    let n = next("2026-10-19T07:00:00Z", &overrides).unwrap();
    assert!(n.is_override);
    assert_eq!(n.event, moved);
    assert_eq!(n.recurrence_id, Some(utc("2026-11-02T08:00:00Z")));
    let n = next("2026-10-30T08:00:00Z", &overrides).unwrap();
    assert_eq!(n.start_utc(Tz::UTC), utc("2026-11-09T08:00:00Z"));

    // Non-recurring.
    let one_off = event(
        "One-off",
        timed("2026-07-01T09:00:00", Berlin),
        timed("2026-07-01T10:00:00", Berlin),
        None,
    );
    let n = next_occurrence(&one_off, &[], utc("2026-07-01T06:59:59Z"), Tz::UTC).unwrap();
    assert_eq!(n.unwrap().recurrence_id, None);
    let n = next_occurrence(&one_off, &[], utc("2026-07-01T07:00:00Z"), Tz::UTC).unwrap();
    assert!(n.is_none());
}

#[test]
fn limit_caps_unbounded_rules() {
    let ev = event(
        "Forever",
        timed("2026-01-01T08:00:00", Berlin),
        timed("2026-01-01T08:15:00", Berlin),
        Some(RRule::new(Frequency::Daily)),
    );

    let opts = range("2026-01-01T00:00:00Z", "2036-01-01T00:00:00Z").with_limit(Some(50));
    let occ = expand(&ev, &[], &opts).unwrap();
    assert_eq!(occ.len(), 50);
    assert_eq!(occ[0].start_utc(Tz::UTC), utc("2026-01-01T07:00:00Z"));
    assert_eq!(occ[49].start_utc(Tz::UTC), utc("2026-02-19T07:00:00Z"));

    // The default limit applies to the full ten years.
    let occ = expand(
        &ev,
        &[],
        &range("2026-01-01T00:00:00Z", "2036-01-01T00:00:00Z"),
    )
    .unwrap();
    assert_eq!(occ.len(), matrix_calendar_sdk::recurrence::DEFAULT_LIMIT);

    // Without a limit, the range bounds the expansion.
    let opts = range("2026-01-01T00:00:00Z", "2027-01-01T00:00:00Z").with_limit(None);
    assert_eq!(expand(&ev, &[], &opts).unwrap().len(), 365);

    // Instances far into an infinite series are reachable.
    let opts = range("2035-06-01T00:00:00Z", "2035-06-03T00:00:00Z");
    assert_eq!(
        starts(&expand(&ev, &[], &opts).unwrap()),
        [utc("2035-06-01T06:00:00Z"), utc("2035-06-02T06:00:00Z")]
    );
}

#[test]
fn until_before_start_yields_only_start_and_rdates() {
    let mut ev = msc_standup();
    if let Some(r) = &mut ev.recurrence {
        r.rrule.until = Some(Until::Date(date("2026-06-30")));
        r.exdates.clear();
        // Duplicates the start, which is emitted once.
        r.rdates = vec![utc("2026-07-01T07:00:00Z"), utc("2026-07-03T07:00:00Z")];
    }
    let occ = expand(
        &ev,
        &[],
        &range("2026-01-01T00:00:00Z", "2027-01-01T00:00:00Z"),
    )
    .unwrap();
    assert_eq!(
        starts(&occ),
        [utc("2026-07-01T07:00:00Z"), utc("2026-07-03T07:00:00Z")]
    );
}

#[test]
fn start_not_matching_rule_counts_toward_count() {
    // Wednesday 2026-07-01 start, weekly on Monday, COUNT=3: the start plus two Mondays.
    let mut ev = msc_standup();
    if let Some(r) = &mut ev.recurrence {
        r.rrule.until = None;
        r.rrule.count = Some(3);
        r.exdates.clear();
    }
    let opts = range("2026-01-01T00:00:00Z", "2027-01-01T00:00:00Z");
    let occ = expand(&ev, &[], &opts).unwrap();
    assert_eq!(
        starts(&occ),
        [
            utc("2026-07-01T07:00:00Z"),
            utc("2026-07-06T07:00:00Z"),
            utc("2026-07-13T07:00:00Z"),
        ]
    );
    assert_eq!(occ[0].recurrence_id, Some(utc("2026-07-01T07:00:00Z")));
    assert!(to_rrule_set(&ev).unwrap().to_string().contains("RDATE"));

    // COUNT=1 is just the start.
    if let Some(r) = &mut ev.recurrence {
        r.rrule.count = Some(1);
    }
    assert_eq!(
        starts(&expand(&ev, &[], &opts).unwrap()),
        [utc("2026-07-01T07:00:00Z")]
    );

    // A start that matches the rule is not counted twice.
    let mut monday = ev.clone();
    monday.start = timed("2026-07-06T09:00:00", Berlin);
    monday.end = timed("2026-07-06T09:30:00", Berlin);
    if let Some(r) = &mut monday.recurrence {
        r.rrule.count = Some(3);
    }
    assert_eq!(
        starts(&expand(&monday, &[], &opts).unwrap()),
        [
            utc("2026-07-06T07:00:00Z"),
            utc("2026-07-13T07:00:00Z"),
            utc("2026-07-20T07:00:00Z"),
        ]
    );
    assert!(!to_rrule_set(&monday).unwrap().to_string().contains("RDATE"));

    // An exdate removes the start like any other instance (it still counts toward COUNT).
    if let Some(r) = &mut ev.recurrence {
        r.rrule.count = Some(3);
        r.exdates = vec![utc("2026-07-01T07:00:00Z")];
    }
    assert_eq!(
        starts(&expand(&ev, &[], &opts).unwrap()),
        [utc("2026-07-06T07:00:00Z"), utc("2026-07-13T07:00:00Z")]
    );
}

#[test]
fn invalid_rules_are_rejected() {
    let ev = event(
        "Bad",
        timed("2026-01-01T08:00:00", Berlin),
        timed("2026-01-01T08:15:00", Berlin),
        Some(RRule {
            bymonthday: Some(vec![1]),
            ..RRule::new(Frequency::Weekly)
        }),
    );
    let opts = range("2026-01-01T00:00:00Z", "2027-01-01T00:00:00Z");
    assert!(matches!(
        expand(&ev, &[], &opts),
        Err(Error::InvalidRecurrence(_))
    ));
    assert!(matches!(
        to_rrule_set(&ev),
        Err(Error::InvalidRecurrence(_))
    ));

    let mut mixed = ev.clone();
    mixed.recurrence = Some(Recurrence::new(RRule::new(Frequency::Daily)));
    mixed.end = EventTime::all_day(date("2026-01-02"));
    assert!(matches!(
        expand(&mixed, &[], &opts),
        Err(Error::InconsistentDateTime(_))
    ));
}
