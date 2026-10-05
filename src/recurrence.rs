//! Recurrence expansion.
//!
//! Given a calendar event with a [`Recurrence`](crate::events::Recurrence) plus its override
//! events, this module produces the concrete [`Occurrence`]s within a UTC range. It is built on
//! the [`rrule`] crate.
//!
//! # Semantics
//!
//! - **Timezone-aware.** Timed series expand with `DTSTART` in the master's named IANA
//!   `timezone`, preserving its stored UTC instant to disambiguate a DST overlap, so a 09:00
//!   `Europe/Berlin` weekly meeting stays at 09:00 local time across DST changes and its UTC
//!   start moves by an hour.
//! - **All-day series** expand on dates; their occurrences are [`EventTime::AllDay`]. Range
//!   checks and ordering place a date at midnight in [`ExpandOptions::fallback_tz`].
//! - **Duration.** Every generated instance has the master's duration. For timed events this
//!   is the exact duration (RFC 5545 §3.3.6: `end.utc - start.utc` of the master), so an
//!   instance spanning a DST transition keeps its length in hours and its local end time
//!   shifts (a nightly 22:00–06:00 Berlin event ends at 05:00 local on the night the clocks go
//!   back). All-day durations are whole days.
//! - **`DTSTART` is always the first instance** (RFC 8984 §4.3.3, RFC 5545 §3.8.5.3), even if
//!   it does not match the rule, and it counts toward `COUNT`. The MSC example — a Wednesday
//!   start with `BYDAY=MO` — therefore has the Wednesday as its first instance, followed by
//!   the Mondays. An exdate can still remove it.
//! - **`UNTIL`.** A date-time `UNTIL` is an inclusive UTC bound. A bare date is inclusive
//!   through the end of that date: for all-day series the date itself is the last possible
//!   instance (RFC 5545); for timed series it is interpreted as 23:59:59 local time on that
//!   date in the event's timezone (RFC 5545 requires a date-time there; this is the lenient
//!   reading). An `UNTIL` before the series start yields no rule instances (only the start
//!   itself and `rdates`), rather than an error.
//! - **`exdates`** remove generated instances (and `rdates`) whose UTC start equals the exdate.
//!   For all-day series an exdate is mapped to a date first (see below). **`rdates`** add
//!   instances; duplicates of rule instances are emitted once.
//! - **Overrides** replace the instance whose original UTC start equals their `recurrence_id`.
//!   The occurrence then carries the override's `start`/`end`/content, its `recurrence_id`
//!   stays the original start, and `is_override` is set. An override with
//!   `status: cancelled` removes the instance. Overrides whose `recurrence_id` matches no
//!   generated instance (for example one excluded by an exdate) are still emitted if they
//!   intersect the range, as CalDAV servers do. If several overrides share a `recurrence_id`,
//!   the last one in the slice wins (callers should resolve edits first).
//!   Overrides are ignored for non-recurring masters.
//! - **All-day instance keys.** An all-day occurrence's `recurrence_id` is midnight UTC of its
//!   date. A UTC instant used as an all-day exdate, rdate, `UNTIL` date-time or override
//!   `recurrence_id` is mapped to a date as follows: if it is midnight in the fallback
//!   timezone, that date; else if it is midnight UTC, the UTC date; else its date in the
//!   fallback timezone. This accepts both "midnight UTC" and "midnight local" encodings.
//! - **Range.** Occurrences are returned when they intersect `[range_start, range_end)`:
//!   `start < range_end && end > range_start`. Zero-duration occurrences are returned when
//!   `range_start <= start < range_end` (as in CalDAV time-range queries, RFC 4791 §9.9).
//!   Results are ordered by UTC start, then by `recurrence_id`.
//! - **Alerts** come from the instance's effective event, so an override's alerts replace the
//!   series' ones; [`Occurrence::alert_times`] resolves them against the instance's times.
//! - **Cancelled masters** are still expanded; check [`CalendarEvent::is_cancelled`] on
//!   [`Occurrence::event`] to filter them.
//! - **Safety caps.** [`ExpandOptions::limit`] caps the number of returned occurrences, and at
//!   most [`MAX_SCANNED_INSTANCES`] rule instances are generated per call (instances before the
//!   range count too), which bounds the cost of pathological rules such as `FREQ=SECONDLY`
//!   starting decades before the range. The `rrule` crate's own iteration limits are enabled
//!   as well, so rules that never match (e.g. `BYMONTH=2;BYMONTHDAY=30`) terminate.
//! - **Supported years.** Series whose start, end or `UNTIL` lie outside the years
//!   [`MIN_YEAR`](crate::events::datetime::MIN_YEAR)`..=`[`MAX_YEAR`](crate::events::datetime::MAX_YEAR)
//!   are rejected with [`Error::InvalidDateTime`]; no input makes expansion panic.

use std::collections::HashMap;

use chrono::{DateTime, Month, NaiveDate, NaiveTime, TimeDelta, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};

use crate::{
    Error, Result,
    events::{
        ByDay, CalendarEvent, EventTime, Frequency, RRule, Until,
        datetime::{check_year, local_naive, resolve_local_lenient},
    },
};

/// Default for [`ExpandOptions::limit`].
pub const DEFAULT_LIMIT: usize = 1000;

/// Maximum number of rule instances generated by a single [`expand`] or [`next_occurrence`]
/// call, including instances before the requested range. Expansion stops silently (with a
/// `tracing` warning) when it is reached.
pub const MAX_SCANNED_INSTANCES: usize = 500_000;

/// 23:59:59, the local time a date-only `UNTIL` stands for with a timed `DTSTART`.
const END_OF_DAY: NaiveTime = match NaiveTime::from_hms_opt(23, 59, 59) {
    Some(t) => t,
    None => NaiveTime::MIN,
};

/// One concrete instance of a (possibly recurring) event.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Occurrence {
    /// The series' `uid`.
    pub uid: String,
    /// Original UTC start of this instance (the `recurrence_id` key that overrides use),
    /// `None` for non-recurring events.
    ///
    /// For all-day series this is midnight UTC of the instance's original date.
    pub recurrence_id: Option<DateTime<Utc>>,
    /// Start of this instance.
    pub start: EventTime,
    /// End of this instance (exclusive for all-day instances).
    pub end: EventTime,
    /// The effective event for this instance: the series master (unchanged, so its
    /// `start`/`end` are those of the first instance), or the override that replaced it.
    pub event: CalendarEvent,
    /// Whether [`Occurrence::event`] is an override.
    pub is_override: bool,
}

impl Occurrence {
    /// The UTC start of this instance; all-day dates resolve to midnight in `fallback_tz`.
    pub fn start_utc(&self, fallback_tz: Tz) -> DateTime<Utc> {
        self.start.to_utc(fallback_tz)
    }

    /// The UTC end of this instance; all-day dates resolve to midnight in `fallback_tz`.
    pub fn end_utc(&self, fallback_tz: Tz) -> DateTime<Utc> {
        self.end.to_utc(fallback_tz)
    }

    /// When the alerts of this instance fire, as `(alert id, fire time)` pairs ordered by
    /// alert ID.
    ///
    /// The alerts are those of [`Occurrence::event`] (so an override's own alerts replace the
    /// series' ones) applied to this instance's `start` and `end`; see
    /// [`Alert::fire_time`](crate::events::Alert::fire_time) for how `fallback_tz` is used.
    /// Alerts in the past are returned as well; it is up to the
    /// caller to filter and schedule them.
    pub fn alert_times(&self, fallback_tz: Tz) -> Vec<(&str, DateTime<Utc>)> {
        self.event
            .alerts
            .iter()
            .map(|(id, alert)| {
                (
                    id.as_str(),
                    alert.fire_time(&self.start, &self.end, fallback_tz),
                )
            })
            .collect()
    }
}

/// Options for [`expand`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExpandOptions {
    /// Inclusive start of the UTC range.
    pub range_start: DateTime<Utc>,
    /// Exclusive end of the UTC range.
    pub range_end: DateTime<Utc>,
    /// Maximum number of occurrences to return (the earliest ones are kept); `None` for no
    /// limit. Defaults to [`DEFAULT_LIMIT`].
    pub limit: Option<usize>,
    /// Timezone used to place all-day dates on the UTC timeline (typically the user's or the
    /// calendar's timezone). Defaults to UTC.
    pub fallback_tz: Tz,
}

impl ExpandOptions {
    /// Options for the range `[range_start, range_end)` with the default limit and UTC as the
    /// fallback timezone.
    pub fn new(range_start: DateTime<Utc>, range_end: DateTime<Utc>) -> Self {
        Self {
            range_start,
            range_end,
            limit: Some(DEFAULT_LIMIT),
            fallback_tz: Tz::UTC,
        }
    }

    /// Sets [`ExpandOptions::limit`].
    pub fn with_limit(mut self, limit: Option<usize>) -> Self {
        self.limit = limit;
        self
    }

    /// Sets [`ExpandOptions::fallback_tz`].
    pub fn with_fallback_tz(mut self, fallback_tz: Tz) -> Self {
        self.fallback_tz = fallback_tz;
        self
    }
}

/// Expands `master` and its `overrides` into the occurrences intersecting
/// `[opts.range_start, opts.range_end)`, ordered by UTC start.
///
/// See the [module documentation](self) for the exact semantics.
///
/// # Errors
///
/// - [`Error::InvalidIdentifier`] if an override's `uid` differs from the master's,
/// - [`Error::InvalidRecurrence`] if an override lacks a `recurrence_id`, or the rule is
///   invalid or rejected by the `rrule` crate,
/// - [`Error::InconsistentDateTime`] if the master's `start` and `end` are not both timed or
///   both all-day,
/// - [`Error::InvalidDateTime`] if a recurring master's `start`, `end` or `UNTIL` lies outside
///   the supported years.
pub fn expand<'a>(
    master: &CalendarEvent,
    overrides: impl IntoIterator<Item = &'a CalendarEvent>,
    opts: &ExpandOptions,
) -> Result<Vec<Occurrence>> {
    let overrides: Vec<&CalendarEvent> = overrides.into_iter().collect();
    check_overrides(master, &overrides)?;
    let fallback_tz = opts.fallback_tz;
    let cap = opts.limit.unwrap_or(usize::MAX);
    let in_range = |start: &EventTime, end: &EventTime| {
        intersects(
            start.to_utc(fallback_tz),
            end.to_utc(fallback_tz),
            opts.range_start,
            opts.range_end,
        )
    };
    if cap == 0 || opts.range_end <= opts.range_start {
        return Ok(Vec::new());
    }

    if master.recurrence.is_none() {
        return Ok(in_range(&master.start, &master.end)
            .then(|| single(master))
            .into_iter()
            .collect());
    }

    let series = Series::build(master, fallback_tz)?;
    let index = OverrideIndex::new(&series.frame, &overrides, fallback_tz);

    let mut out = Vec::new();
    for instance in series.instances() {
        if instance.start.to_utc(fallback_tz) >= opts.range_end {
            break;
        }
        if index.replaces(&instance) || !in_range(&instance.start, &instance.end) {
            continue;
        }
        out.push(instance.into_occurrence(master));
        if out.len() >= cap {
            break;
        }
    }
    out.extend(
        index
            .effective()
            .filter(|o| in_range(&o.start, &o.end))
            .map(override_occurrence),
    );

    out.sort_by_cached_key(|o| (o.start_utc(fallback_tz), o.recurrence_id));
    out.truncate(cap);
    Ok(out)
}

/// The first occurrence of `master` (with `overrides` applied) that starts strictly after
/// `after`, or `None` if there is none.
///
/// Occurrences already in progress at `after` are not returned. All-day dates are placed on
/// the timeline at midnight in `fallback_tz`.
///
/// # Errors
///
/// As for [`expand`].
pub fn next_occurrence<'a>(
    master: &CalendarEvent,
    overrides: impl IntoIterator<Item = &'a CalendarEvent>,
    after: DateTime<Utc>,
    fallback_tz: Tz,
) -> Result<Option<Occurrence>> {
    let overrides: Vec<&CalendarEvent> = overrides.into_iter().collect();
    check_overrides(master, &overrides)?;

    if master.recurrence.is_none() {
        return Ok((master.start.to_utc(fallback_tz) > after).then(|| single(master)));
    }

    let series = Series::build(master, fallback_tz)?;
    let index = OverrideIndex::new(&series.frame, &overrides, fallback_tz);

    let generated = series
        .instances()
        .filter(|i| i.start.to_utc(fallback_tz) > after && !index.replaces(i))
        .map(|i| i.into_occurrence(master))
        .next();
    let overridden = index
        .effective()
        .filter(|o| o.start.to_utc(fallback_tz) > after)
        .map(override_occurrence)
        .min_by_key(|o| (o.start_utc(fallback_tz), o.recurrence_id));

    Ok(match (generated, overridden) {
        (Some(g), Some(o)) => Some(
            if (o.start_utc(fallback_tz), o.recurrence_id)
                < (g.start_utc(fallback_tz), g.recurrence_id)
            {
                o
            } else {
                g
            },
        ),
        (g, o) => g.or(o),
    })
}

/// Converts the recurrence of `master` (rule, `exdates` and `rdates`) into an
/// [`rrule::RRuleSet`].
///
/// Timed series use the master's stored UTC instant in its named timezone as `DTSTART`. All-day
/// series are expressed as midnight-UTC date-times standing for floating dates; all-day
/// exdates/rdates are mapped to dates with UTC as the fallback timezone. If `DTSTART` does not
/// match the rule it is added as an `RDATE` (and `COUNT` reduced by one), since it is always
/// the first instance (see the [module documentation](self)). The `rrule` crate's iteration
/// limits are enabled on the returned set. Overrides are not part of the set; use [`expand`]
/// for those.
///
/// # Errors
///
/// [`Error::InvalidRecurrence`] if `master` has no recurrence, or the rule is invalid or
/// rejected by the `rrule` crate; [`Error::InconsistentDateTime`] if `start` and `end` are not
/// both timed or both all-day.
pub fn to_rrule_set(master: &CalendarEvent) -> Result<rrule::RRuleSet> {
    Series::build(master, Tz::UTC).map(|s| s.set)
}

// ---------------------------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------------------------

/// Whether `[start, end)` intersects `[range_start, range_end)`, treating zero-duration
/// (or inverted) intervals as points.
fn intersects(
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    range_start: DateTime<Utc>,
    range_end: DateTime<Utc>,
) -> bool {
    if end <= start {
        range_start <= start && start < range_end
    } else {
        start < range_end && end > range_start
    }
}

/// The single occurrence of a non-recurring event.
fn single(master: &CalendarEvent) -> Occurrence {
    Occurrence {
        uid: master.uid.clone(),
        recurrence_id: None,
        start: master.start,
        end: master.end,
        event: master.clone(),
        is_override: false,
    }
}

fn override_occurrence(o: &CalendarEvent) -> Occurrence {
    Occurrence {
        uid: o.uid.clone(),
        recurrence_id: o.recurrence_id,
        start: o.start,
        end: o.end,
        event: o.clone(),
        is_override: true,
    }
}

fn check_overrides(master: &CalendarEvent, overrides: &[&CalendarEvent]) -> Result<()> {
    for o in overrides {
        if o.uid != master.uid {
            return Err(Error::InvalidIdentifier(format!(
                "override uid {} does not match series uid {}",
                o.uid, master.uid
            )));
        }
        if o.recurrence_id.is_none() {
            return Err(Error::InvalidRecurrence(format!(
                "override of {} has no recurrence_id",
                o.uid
            )));
        }
    }
    Ok(())
}

/// Midnight UTC of `date`.
fn midnight_utc(date: NaiveDate) -> DateTime<Utc> {
    date.and_time(NaiveTime::MIN).and_utc()
}

/// Maps a UTC instant used in an all-day context to a date (see the module docs).
fn instant_to_date(instant: DateTime<Utc>, fallback_tz: Tz) -> NaiveDate {
    let local = local_naive(instant, fallback_tz);
    if local.time() == NaiveTime::MIN {
        local.date()
    } else if instant.time() == NaiveTime::MIN {
        instant.naive_utc().date()
    } else {
        local.date()
    }
}

/// How generated instances map back to event times.
#[derive(Clone, Copy, Debug)]
enum Frame {
    /// A timed series expanded in `tz`.
    Timed {
        /// Timezone of `start` (the expansion timezone).
        tz: Tz,
        /// Timezone of `end`.
        end_tz: Tz,
        /// Exact duration (RFC 5545 §3.3.6), never negative.
        exact: TimeDelta,
    },
    /// An all-day series, expanded as midnight-UTC date-times standing for floating dates.
    AllDay {
        /// Duration in days.
        days: i64,
    },
}

impl Frame {
    /// Converts a UTC instant used as exdate/rdate into the series' frame.
    fn frame_instant(&self, instant: DateTime<Utc>, fallback_tz: Tz) -> DateTime<rrule::Tz> {
        match self {
            Self::Timed { tz, .. } => instant.with_timezone(&rrule::Tz::Tz(*tz)),
            Self::AllDay { .. } => {
                midnight_utc(instant_to_date(instant, fallback_tz)).with_timezone(&rrule::Tz::UTC)
            }
        }
    }

    /// The override-matching key of an override's `recurrence_id`.
    fn override_key(&self, recurrence_id: DateTime<Utc>, fallback_tz: Tz) -> i64 {
        match self {
            Self::Timed { .. } => recurrence_id.timestamp(),
            Self::AllDay { .. } => {
                midnight_utc(instant_to_date(recurrence_id, fallback_tz)).timestamp()
            }
        }
    }

    /// Builds the instance for a date-time generated by the `rrule` crate.
    fn instance(&self, generated: DateTime<rrule::Tz>) -> Instance {
        match *self {
            Self::Timed { tz, end_tz, exact } => {
                let start = EventTime::from_utc(generated.with_timezone(&Utc), tz);
                let start_utc = start.to_utc(tz);
                let end_utc = start_utc.checked_add_signed(exact).unwrap_or(start_utc);
                Instance {
                    recurrence_id: start_utc,
                    start,
                    end: EventTime::from_utc(end_utc, end_tz),
                }
            }
            Self::AllDay { days } => {
                let date = generated.date_naive();
                let end = TimeDelta::try_days(days)
                    .and_then(|d| date.checked_add_signed(d))
                    .unwrap_or(date);
                Instance {
                    recurrence_id: midnight_utc(date),
                    start: EventTime::all_day(date),
                    end: EventTime::all_day(end),
                }
            }
        }
    }
}

/// A generated (not yet override-resolved) instance.
struct Instance {
    recurrence_id: DateTime<Utc>,
    start: EventTime,
    end: EventTime,
}

impl Instance {
    fn into_occurrence(self, master: &CalendarEvent) -> Occurrence {
        Occurrence {
            uid: master.uid.clone(),
            recurrence_id: Some(self.recurrence_id),
            start: self.start,
            end: self.end,
            event: master.clone(),
            is_override: false,
        }
    }
}

/// A recurring master converted for the `rrule` crate.
struct Series {
    set: rrule::RRuleSet,
    frame: Frame,
}

impl Series {
    fn build(master: &CalendarEvent, fallback_tz: Tz) -> Result<Self> {
        let recurrence = master.recurrence.as_ref().ok_or_else(|| {
            Error::InvalidRecurrence(format!("event {} has no recurrence", master.uid))
        })?;
        let rule = &recurrence.rrule;
        rule.validate()?;
        // Keeps all date arithmetic below (and in the `rrule` crate) far from chrono's limits.
        master.start.check_year_range()?;
        master.end.check_year_range()?;
        match rule.until {
            Some(Until::Date(date)) => check_year(date)?,
            Some(Until::DateTime(utc)) => check_year(utc.naive_utc().date())?,
            None => {}
        }

        let (dt_start, frame) = match (&master.start, &master.end) {
            (
                EventTime::Timed {
                    utc: start_utc,
                    timezone,
                    ..
                },
                EventTime::Timed {
                    utc: end_utc,
                    timezone: end_tz,
                    ..
                },
            ) => {
                // The stored instants disambiguate a DST overlap. Resolving the local
                // times again would move a later-fold start and change the duration.
                let frame = Frame::Timed {
                    tz: *timezone,
                    end_tz: *end_tz,
                    exact: end_utc
                        .signed_duration_since(*start_utc)
                        .max(TimeDelta::zero()),
                };
                (start_utc.with_timezone(&rrule::Tz::Tz(*timezone)), frame)
            }
            (EventTime::AllDay { date }, EventTime::AllDay { date: end }) => {
                let days = end.signed_duration_since(*date).num_days().max(1);
                (
                    midnight_utc(*date).with_timezone(&rrule::Tz::UTC),
                    Frame::AllDay { days },
                )
            }
            _ => {
                return Err(Error::InconsistentDateTime(
                    "start and end must both be timed or both be all-day".into(),
                ));
            }
        };

        let until = rule.until.map(|until| match (until, &frame) {
            (Until::DateTime(utc), Frame::Timed { .. }) => utc,
            (Until::Date(date), Frame::Timed { tz, .. }) => {
                resolve_local_lenient(date.and_time(END_OF_DAY), *tz)
            }
            (Until::Date(date), Frame::AllDay { .. }) => midnight_utc(date),
            (Until::DateTime(utc), Frame::AllDay { .. }) => {
                midnight_utc(instant_to_date(utc, fallback_tz))
            }
        });

        // The rule with `UNTIL` applied, or `None` if it produces no instances.
        let bounded = |rule: &RRule| -> Result<Option<rrule::RRule>> {
            let unbounded = convert_rule(rule)?;
            Ok(match until {
                Some(until) if until < dt_start.with_timezone(&Utc) => {
                    // Check the rest of the rule anyway, but it produces no instances.
                    validate_rule(unbounded, dt_start)?;
                    None
                }
                Some(until) => Some(validate_rule(
                    unbounded.until(until.with_timezone(&rrule::Tz::UTC)),
                    dt_start,
                )?),
                None => Some(validate_rule(unbounded, dt_start)?),
            })
        };
        let mut validated = bounded(rule)?;

        let mut rdates: Vec<DateTime<rrule::Tz>> = recurrence
            .rdates
            .iter()
            .map(|r| frame.frame_instant(*r, fallback_tz))
            .collect();
        // DTSTART is always the first instance and counts toward COUNT (RFC 8984 §4.3.3,
        // RFC 5545 §3.8.5.3). If the rule does not generate it, add it as an RDATE and let the
        // rule produce one instance less.
        let rule_starts_at_dtstart = validated.as_ref().is_some_and(|v| {
            let only_rule = rrule::RRuleSet::new(dt_start).limit().rrule(v.clone());
            (&only_rule).into_iter().next() == Some(dt_start)
        });
        if !rule_starts_at_dtstart {
            rdates.push(dt_start);
            if let Some(count) = rule.count {
                validated = match count.saturating_sub(1) {
                    0 => None,
                    rest => bounded(&RRule {
                        count: Some(rest),
                        ..rule.clone()
                    })?,
                };
            }
        }

        let mut set = rrule::RRuleSet::new(dt_start).limit();
        if let Some(validated) = validated {
            set = set.rrule(validated);
        }
        set = set
            .set_exdates(
                recurrence
                    .exdates
                    .iter()
                    .map(|e| frame.frame_instant(*e, fallback_tz))
                    .collect(),
            )
            .set_rdates(rdates);
        Ok(Self { set, frame })
    }

    /// The generated instances in ascending order, deduplicated, at most
    /// [`MAX_SCANNED_INSTANCES`].
    fn instances(&self) -> impl Iterator<Item = Instance> + '_ {
        let mut scanned = 0usize;
        let mut last_key: Option<i64> = None;
        (&self.set)
            .into_iter()
            .take_while(move |_| {
                scanned += 1;
                if scanned > MAX_SCANNED_INSTANCES {
                    tracing::warn!(
                        "recurrence expansion stopped after {MAX_SCANNED_INSTANCES} instances"
                    );
                    false
                } else {
                    true
                }
            })
            .map(|generated| self.frame.instance(generated))
            .filter(move |instance| {
                let key = instance.recurrence_id.timestamp();
                let duplicate = last_key == Some(key);
                last_key = Some(key);
                !duplicate
            })
    }
}

fn validate_rule(
    rule: rrule::RRule<rrule::Unvalidated>,
    dt_start: DateTime<rrule::Tz>,
) -> Result<rrule::RRule> {
    rule.validate(dt_start)
        .map_err(|e| Error::InvalidRecurrence(e.to_string()))
}

/// Converts every part of the JSON rule except `UNTIL` (which depends on the frame).
fn convert_rule(rule: &RRule) -> Result<rrule::RRule<rrule::Unvalidated>> {
    fn narrow<S: Copy + std::fmt::Display, T: TryFrom<S>>(
        part: &str,
        values: &[S],
    ) -> Result<Vec<T>> {
        values
            .iter()
            .map(|v| {
                T::try_from(*v).map_err(|_| {
                    Error::InvalidRecurrence(format!("{part} value {v} is out of range"))
                })
            })
            .collect()
    }

    let freq = match &rule.freq {
        Frequency::Secondly => rrule::Frequency::Secondly,
        Frequency::Minutely => rrule::Frequency::Minutely,
        Frequency::Hourly => rrule::Frequency::Hourly,
        Frequency::Daily => rrule::Frequency::Daily,
        Frequency::Weekly => rrule::Frequency::Weekly,
        Frequency::Monthly => rrule::Frequency::Monthly,
        Frequency::Yearly => rrule::Frequency::Yearly,
        other => {
            return Err(Error::InvalidRecurrence(format!(
                "unsupported FREQ {other}"
            )));
        }
    };
    let mut out = rrule::RRule::new(freq);
    if let Some(interval) = rule.interval {
        out =
            out.interval(u16::try_from(interval).map_err(|_| {
                Error::InvalidRecurrence(format!("INTERVAL {interval} is too large"))
            })?);
    }
    if let Some(count) = rule.count {
        out = out.count(count);
    }
    if let Some(values) = &rule.bysecond {
        out = out.by_second(narrow("BYSECOND", values)?);
    }
    if let Some(values) = &rule.byminute {
        out = out.by_minute(narrow("BYMINUTE", values)?);
    }
    if let Some(values) = &rule.byhour {
        out = out.by_hour(narrow("BYHOUR", values)?);
    }
    if let Some(values) = &rule.byday {
        out = out.by_weekday(
            values
                .iter()
                .map(|&ByDay { ordinal, weekday }| {
                    rrule::NWeekday::new(ordinal.map(i16::from), weekday.into())
                })
                .collect(),
        );
    }
    if let Some(values) = &rule.bymonthday {
        out = out.by_month_day(narrow("BYMONTHDAY", values)?);
    }
    if let Some(values) = &rule.byyearday {
        out = out.by_year_day(narrow("BYYEARDAY", values)?);
    }
    if let Some(values) = &rule.byweekno {
        out = out.by_week_no(narrow("BYWEEKNO", values)?);
    }
    if let Some(values) = &rule.bymonth {
        let months = values
            .iter()
            .map(|m| {
                u8::try_from(*m)
                    .ok()
                    .and_then(|m| Month::try_from(m).ok())
                    .ok_or_else(|| {
                        Error::InvalidRecurrence(format!("BYMONTH value {m} is invalid"))
                    })
            })
            .collect::<Result<Vec<_>>>()?;
        out = out.by_month(&months);
    }
    if let Some(values) = &rule.bysetpos {
        out = out.by_set_pos(values.clone());
    }
    if let Some(wkst) = rule.wkst {
        out = out.week_start(wkst.into());
    }
    Ok(out)
}

/// Overrides of one series, keyed by the instance they replace.
struct OverrideIndex<'a> {
    by_key: HashMap<i64, &'a CalendarEvent>,
}

impl<'a> OverrideIndex<'a> {
    /// Indexes `overrides` (already checked to have a `recurrence_id`); later entries win.
    fn new(frame: &Frame, overrides: &[&'a CalendarEvent], fallback_tz: Tz) -> Self {
        let by_key = overrides
            .iter()
            .copied()
            .filter_map(|o| {
                o.recurrence_id
                    .map(|rid| (frame.override_key(rid, fallback_tz), o))
            })
            .collect();
        Self { by_key }
    }

    /// Whether a (cancelled or replacing) override exists for `instance`.
    fn replaces(&self, instance: &Instance) -> bool {
        self.by_key
            .contains_key(&instance.recurrence_id.timestamp())
    }

    /// The overrides that produce an occurrence (i.e. are not cancelled).
    fn effective(&self) -> impl Iterator<Item = &'a CalendarEvent> + '_ {
        self.by_key.values().copied().filter(|o| !o.is_cancelled())
    }
}

#[cfg(test)]
mod tests {
    use chrono_tz::Europe::Berlin;
    use matrix_sdk::ruma::owned_user_id;

    use super::*;
    use crate::events::{Recurrence, Weekday};

    fn utc(s: &str) -> DateTime<Utc> {
        crate::events::datetime::parse_utc(s).expect("valid timestamp")
    }

    fn timed(local: &str, tz: Tz) -> EventTime {
        EventTime::timed(local.parse().expect("valid localtime"), tz).expect("exists")
    }

    fn daily(start: &str, end: &str, rule: RRule) -> CalendarEvent {
        let mut ev = CalendarEvent::new(
            "Daily",
            timed(start, Berlin),
            timed(end, Berlin),
            owned_user_id!("@alice:example.com"),
        );
        ev.recurrence = Some(Recurrence::new(rule));
        ev
    }

    #[test]
    fn instance_in_dst_gap_moves_after_gap() {
        // Berlin springs forward at 2026-03-29 02:00 -> 03:00.
        let ev = daily(
            "2026-03-28T02:30:00",
            "2026-03-28T03:00:00",
            RRule {
                count: Some(3),
                ..RRule::new(Frequency::Daily)
            },
        );
        let occ = expand(
            &ev,
            &[],
            &ExpandOptions::new(utc("2026-03-01T00:00:00Z"), utc("2026-04-01T00:00:00Z")),
        )
        .expect("expands");
        let starts: Vec<_> = occ.iter().map(|o| o.start_utc(Tz::UTC)).collect();
        assert_eq!(
            starts,
            [
                utc("2026-03-28T01:30:00Z"),
                utc("2026-03-29T01:30:00Z"), // 03:30 CEST
                utc("2026-03-30T00:30:00Z"),
            ]
        );
        // Exact duration: 01:30Z + 30 min = 02:00Z, i.e. 04:00 CEST.
        assert_eq!(occ[1].end_utc(Tz::UTC), utc("2026-03-29T02:00:00Z"));
        for o in &occ {
            o.start.validate().expect("consistent start");
            o.end.validate().expect("consistent end");
        }
    }

    #[test]
    fn overnight_event_keeps_exact_duration_across_fall_back() {
        // 22:00 - 06:00 (8 hours); Berlin falls back on 2026-10-25 03:00 -> 02:00.
        let ev = daily(
            "2026-10-23T22:00:00",
            "2026-10-24T06:00:00",
            RRule {
                count: Some(3),
                ..RRule::new(Frequency::Daily)
            },
        );
        let occ = expand(
            &ev,
            &[],
            &ExpandOptions::new(utc("2026-10-01T00:00:00Z"), utc("2026-11-01T00:00:00Z")),
        )
        .expect("expands");
        assert_eq!(occ.len(), 3);
        // The night of the transition keeps its 8 hours and therefore ends at 05:00 local.
        assert_eq!(
            occ[1].end,
            EventTime::Timed {
                utc: utc("2026-10-25T04:00:00Z"),
                localtime: "2026-10-25T05:00:00".parse().expect("valid"),
                timezone: Berlin,
            }
        );
        // The next night is back at 06:00 local.
        assert_eq!(occ[2].end_utc(Tz::UTC), utc("2026-10-26T05:00:00Z"));
        for o in &occ {
            assert_eq!(
                o.end_utc(Tz::UTC) - o.start_utc(Tz::UTC),
                TimeDelta::hours(8)
            );
            o.end.validate().expect("consistent end");
        }
    }

    #[test]
    fn instant_to_date_prefers_exact_midnights() {
        let d = |s: &str| s.parse::<NaiveDate>().expect("valid date");
        assert_eq!(
            instant_to_date(utc("2026-12-24T23:00:00Z"), Berlin),
            d("2026-12-25")
        );
        assert_eq!(
            instant_to_date(utc("2026-12-25T00:00:00Z"), Berlin),
            d("2026-12-25")
        );
        assert_eq!(
            instant_to_date(utc("2026-12-25T00:00:00Z"), chrono_tz::America::New_York),
            d("2026-12-25")
        );
        assert_eq!(
            instant_to_date(utc("2026-12-25T12:00:00Z"), Berlin),
            d("2026-12-25")
        );
    }

    #[test]
    fn rule_conversion_errors() {
        let mut ev = daily(
            "2026-01-01T09:00:00",
            "2026-01-01T10:00:00",
            RRule {
                interval: Some(70_000),
                ..RRule::new(Frequency::Daily)
            },
        );
        assert!(matches!(
            to_rrule_set(&ev),
            Err(Error::InvalidRecurrence(_))
        ));

        // Accepted by our validation, rejected by the rrule crate.
        ev.recurrence = Some(Recurrence::new(RRule {
            bysecond: Some(vec![60]),
            ..RRule::new(Frequency::Daily)
        }));
        assert!(matches!(
            to_rrule_set(&ev),
            Err(Error::InvalidRecurrence(_))
        ));

        ev.recurrence = Some(Recurrence::new(RRule {
            byday: Some(vec![ByDay::nth(1, Weekday::Monday)]),
            ..RRule::new(Frequency::Weekly)
        }));
        assert!(matches!(
            to_rrule_set(&ev),
            Err(Error::InvalidRecurrence(_))
        ));
    }
}
