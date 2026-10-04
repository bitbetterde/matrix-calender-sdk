//! Availability (MSC4496 §4).
//!
//! - Client-side busy/free computation from expanded occurrences
//!   ([`compute_busy_from_occurrences`]) or from single, non-recurring events
//!   ([`compute_busy`]), and [`merge_intervals`].
//! - Working hours (RFC 7953 `VAVAILABILITY` semantics): [`unavailable_from_windows`].
//! - Tier evaluation of an [`AvailabilityPolicyEventContent`]: [`evaluate_tier`].
//! - Reading and writing the policy and window state events of a calendar room:
//!   [`get_policy`], [`set_policy`], [`get_window`], [`set_window`].
//! - The (unstable) client-server availability query: [`query_availability`], built on the
//!   endpoint definition in [`endpoint`].

use chrono::{DateTime, Datelike, Days, NaiveDate, Utc};
use chrono_tz::Tz;
use matrix_sdk::{
    Client, Room,
    deserialized_responses::RawAnySyncOrStrippedState,
    ruma::{
        MilliSecondsSinceUnixEpoch, OwnedEventId, UserId, api::error::ErrorKind,
        events::StateEventType,
    },
};
use serde::{Deserialize, de::DeserializeOwned, de::IgnoredAny};
use tracing::warn;

use crate::{
    Error, Result,
    events::{
        AvailabilityPolicyEventContent, AvailabilityRequest, AvailabilityResponse,
        AvailabilityWindowEventContent, BusyInterval, BusyType, CalendarEvent, EventTime, Status,
        Transparency, Weekday,
        datetime::{local_naive, resolve_local_lenient},
    },
    ids,
    recurrence::Occurrence,
};

/// Computes the busy intervals within `[range_start, range_end)` from single, non-recurring
/// events, each spanning its own `start`/`end`.
///
/// **Recurring series must be expanded first**: a series master (an event with a
/// `recurrence`) only describes its first instance, so it is skipped with a warning rather than
/// being counted once. Expand series with the [`recurrence`](crate::recurrence) module and pass
/// the occurrences to [`compute_busy_from_occurrences`] instead (or use
/// [`CalendarIndex::busy`](crate::calendar::CalendarIndex::busy), which does both).
///
/// All-day events are resolved in `fallback_tz`.
///
/// - Transparent and cancelled events never appear (MSC4496 §4.5).
/// - Tentative events give [`BusyType::BusyTentative`], all others [`BusyType::Busy`].
/// - Intervals are clipped to the range; empty intervals are dropped.
/// - With `include_titles` (Tier 2), each interval carries its event's title and intervals are
///   not merged. Otherwise overlapping or adjacent intervals of the same type are merged (see
///   [`merge_intervals`]).
/// - The result is sorted by start.
pub fn compute_busy<'a>(
    events: impl IntoIterator<Item = &'a CalendarEvent>,
    range_start: DateTime<Utc>,
    range_end: DateTime<Utc>,
    fallback_tz: Tz,
    include_titles: bool,
) -> Vec<BusyInterval> {
    let instances = events.into_iter().filter(|ev| {
        if ev.recurrence.is_some() {
            warn!(
                uid = ev.uid,
                "compute_busy: skipping recurring series; expand it into occurrences first"
            );
            false
        } else {
            true
        }
    });
    busy_from_instances(
        instances.map(|ev| (ev, &ev.start, &ev.end)),
        range_start,
        range_end,
        fallback_tz,
        include_titles,
    )
}

/// Computes the busy intervals within `[range_start, range_end)` from expanded occurrences
/// (as returned by [`recurrence::expand`](crate::recurrence::expand)).
///
/// Each occurrence counts with its own `start`/`end`, and the status, transparency and title
/// of its effective event ([`Occurrence::event`], the master or the override). Otherwise the
/// rules of [`compute_busy`] apply.
pub fn compute_busy_from_occurrences<'a>(
    occurrences: impl IntoIterator<Item = &'a Occurrence>,
    range_start: DateTime<Utc>,
    range_end: DateTime<Utc>,
    fallback_tz: Tz,
    include_titles: bool,
) -> Vec<BusyInterval> {
    busy_from_instances(
        occurrences
            .into_iter()
            .map(|occ| (&occ.event, &occ.start, &occ.end)),
        range_start,
        range_end,
        fallback_tz,
        include_titles,
    )
}

fn busy_from_instances<'a>(
    instances: impl Iterator<Item = (&'a CalendarEvent, &'a EventTime, &'a EventTime)>,
    range_start: DateTime<Utc>,
    range_end: DateTime<Utc>,
    fallback_tz: Tz,
    include_titles: bool,
) -> Vec<BusyInterval> {
    let mut intervals: Vec<BusyInterval> = instances
        .filter(|(ev, _, _)| ev.transparency != Transparency::Transparent)
        .filter(|(ev, _, _)| ev.status != Status::Cancelled)
        .filter_map(|(ev, start, end)| {
            let start = start.to_utc(fallback_tz).max(range_start);
            let end = end.to_utc(fallback_tz).min(range_end);
            (start < end).then(|| BusyInterval {
                start,
                end,
                kind: match ev.status {
                    Status::Tentative => BusyType::BusyTentative,
                    _ => BusyType::Busy,
                },
                title: include_titles.then(|| ev.title.clone()),
            })
        })
        .collect();

    if include_titles {
        sort_intervals(&mut intervals);
        intervals
    } else {
        merge_intervals(intervals)
    }
}

/// Merges overlapping or adjacent intervals that have the same type and the same title, and
/// returns the result sorted by start (then end, then type).
///
/// Intervals of different types are kept separate even if they overlap, as iCalendar
/// `FREEBUSY` allows. Empty intervals (`end <= start`) are dropped.
pub fn merge_intervals(mut intervals: Vec<BusyInterval>) -> Vec<BusyInterval> {
    intervals.retain(|i| i.start < i.end);
    intervals.sort_by(|a, b| {
        (busy_type_rank(&a.kind), &a.title, a.start, a.end).cmp(&(
            busy_type_rank(&b.kind),
            &b.title,
            b.start,
            b.end,
        ))
    });

    let mut merged: Vec<BusyInterval> = Vec::with_capacity(intervals.len());
    for interval in intervals {
        match merged.last_mut() {
            Some(last)
                if last.kind == interval.kind
                    && last.title == interval.title
                    && interval.start <= last.end =>
            {
                last.end = last.end.max(interval.end);
            }
            _ => merged.push(interval),
        }
    }
    sort_intervals(&mut merged);
    merged
}

fn busy_type_rank(kind: &BusyType) -> (u8, &str) {
    match kind {
        BusyType::Busy => (0, ""),
        BusyType::BusyUnavailable => (1, ""),
        BusyType::BusyTentative => (2, ""),
        other => (3, other.as_str()),
    }
}

fn sort_intervals(intervals: &mut [BusyInterval]) {
    intervals.sort_by(|a, b| {
        (a.start, a.end, busy_type_rank(&a.kind), &a.title).cmp(&(
            b.start,
            b.end,
            busy_type_rank(&b.kind),
            &b.title,
        ))
    });
}

/// Returns [`BusyType::BusyUnavailable`] intervals covering the parts of
/// `[range_start, range_end)` that lie **outside** the working-hours windows, as RFC 7953
/// `VAVAILABILITY` describes. Returns an empty list if no windows are configured.
///
/// Each window is evaluated in its own timezone on every listed weekday, so the UTC times shift
/// with daylight saving time. A wall-clock time that does not exist on a given day (a DST gap)
/// is resolved as RFC 5545 prescribes (using the offset before the gap); an ambiguous one
/// resolves to the earlier instant. A window whose `end_time` is not after its `start_time`
/// ends on the following day (so `00:00`–`00:00` covers the whole day).
pub fn unavailable_from_windows(
    windows: &AvailabilityWindowEventContent,
    range_start: DateTime<Utc>,
    range_end: DateTime<Utc>,
) -> Vec<BusyInterval> {
    if windows.windows.is_empty() || range_start >= range_end {
        return Vec::new();
    }

    // The available (working) intervals, as (start, end) pairs.
    let mut available: Vec<(DateTime<Utc>, DateTime<Utc>)> = Vec::new();
    for window in &windows.windows {
        let tz = window.timezone;
        let overnight = window.end_time <= window.start_time;
        // One extra day on each side covers windows crossing midnight and any UTC offset
        // (saturating at the ends of chrono's range).
        let first = local_date(range_start, tz);
        let first = first.checked_sub_days(Days::new(1)).unwrap_or(first);
        let last = local_date(range_end, tz);
        let last = last.checked_add_days(Days::new(1)).unwrap_or(last);
        for date in first.iter_days().take_while(|d| *d <= last) {
            if !window.days.contains(&Weekday::from(date.weekday())) {
                continue;
            }
            let start = resolve_local_lenient(date.and_time(window.start_time), tz);
            let end_date = if overnight {
                date.checked_add_days(Days::new(1)).unwrap_or(date)
            } else {
                date
            };
            let end = resolve_local_lenient(end_date.and_time(window.end_time), tz);
            let (start, end) = (start.max(range_start), end.min(range_end));
            if start < end {
                available.push((start, end));
            }
        }
    }
    available.sort();

    // The complement of the union of the available intervals within the range.
    let mut unavailable = Vec::new();
    let mut cursor = range_start;
    for (start, end) in available {
        if start > cursor {
            unavailable.push(unavailable_interval(cursor, start));
        }
        cursor = cursor.max(end);
    }
    if cursor < range_end {
        unavailable.push(unavailable_interval(cursor, range_end));
    }
    unavailable
}

fn local_date(instant: DateTime<Utc>, tz: Tz) -> NaiveDate {
    local_naive(instant, tz).date()
}

fn unavailable_interval(start: DateTime<Utc>, end: DateTime<Utc>) -> BusyInterval {
    BusyInterval {
        start,
        end,
        kind: BusyType::BusyUnavailable,
        title: None,
    }
}

/// Evaluates the availability tier `requester` gets under `policy` (MSC4496 §4.1, §4.2).
///
/// A grant matches if its `subject` is exactly the requester's user ID, or if it is a server
/// wildcard `*:server` and `server` is the requester's server name (compared
/// case-insensitively, as server names are DNS names). The highest tier among the matching
/// grants wins, even if it is lower than `default_tier` (an explicit grant overrides the
/// default). Without a matching grant, `default_tier` applies.
pub fn evaluate_tier(policy: &AvailabilityPolicyEventContent, requester: &UserId) -> u8 {
    policy
        .grants
        .iter()
        .filter(|grant| grant_matches(&grant.subject, requester))
        .map(|grant| grant.tier)
        .max()
        .unwrap_or(policy.default_tier)
}

fn grant_matches(subject: &str, requester: &UserId) -> bool {
    match subject.strip_prefix("*:") {
        Some(server) => server.eq_ignore_ascii_case(requester.server_name().as_str()),
        None => subject == requester.as_str(),
    }
}

/// Reads the availability policy state event of the calendar room `room` from the local
/// state store.
///
/// Both the stable (`m.calendar.availability_policy`) and the unstable type are accepted; if
/// both exist the more recent one wins (the unstable one on a tie). Returns `None` if neither
/// exists or the event was redacted.
pub async fn get_policy(room: &Room) -> Result<Option<AvailabilityPolicyEventContent>> {
    get_state_content(
        room,
        ids::AVAILABILITY_POLICY_UNSTABLE,
        ids::AVAILABILITY_POLICY,
    )
    .await
}

/// Sends the availability policy state event (unstable type, empty state key) into the
/// calendar room `room`.
///
/// Fails with [`Error::PermissionDenied`] without sending
/// anything if the locally synced power levels do not allow it (by default only calendar
/// owners may).
pub async fn set_policy(
    room: &Room,
    policy: AvailabilityPolicyEventContent,
) -> Result<OwnedEventId> {
    crate::permissions::ensure_can_send_state(
        room,
        ids::AVAILABILITY_POLICY_UNSTABLE,
        "change the availability policy",
    )
    .await?;
    Ok(room.send_state_event(policy).await?.event_id)
}

/// Reads the working-hours window state event of the calendar room `room` from the local
/// state store.
///
/// Both the stable (`m.calendar.availability_window`) and the unstable type are accepted; if
/// both exist the more recent one wins (the unstable one on a tie). Returns `None` if neither
/// exists or the event was redacted.
pub async fn get_window(room: &Room) -> Result<Option<AvailabilityWindowEventContent>> {
    get_state_content(
        room,
        ids::AVAILABILITY_WINDOW_UNSTABLE,
        ids::AVAILABILITY_WINDOW,
    )
    .await
}

/// Sends the working-hours window state event (unstable type, empty state key) into the
/// calendar room `room`.
///
/// Fails with [`Error::PermissionDenied`] without sending
/// anything if the locally synced power levels do not allow it (by default only calendar
/// owners may).
pub async fn set_window(
    room: &Room,
    window: AvailabilityWindowEventContent,
) -> Result<OwnedEventId> {
    crate::permissions::ensure_can_send_state(
        room,
        ids::AVAILABILITY_WINDOW_UNSTABLE,
        "change the availability window",
    )
    .await?;
    Ok(room.send_state_event(window).await?.event_id)
}

#[derive(Deserialize)]
struct RawState {
    #[serde(default)]
    content: serde_json::Value,
    origin_server_ts: Option<MilliSecondsSinceUnixEpoch>,
    unsigned: Option<RawUnsigned>,
}

#[derive(Deserialize)]
struct RawUnsigned {
    redacted_because: Option<IgnoredAny>,
}

async fn read_state(room: &Room, event_type: &str) -> Result<Option<RawState>> {
    let Some(raw) = room
        .get_state_event(StateEventType::from(event_type), "")
        .await?
    else {
        return Ok(None);
    };
    let parsed: RawState = match &raw {
        RawAnySyncOrStrippedState::Sync(raw) => raw.deserialize_as_unchecked()?,
        RawAnySyncOrStrippedState::Stripped(raw) => raw.deserialize_as_unchecked()?,
    };
    let redacted = parsed
        .unsigned
        .as_ref()
        .is_some_and(|u| u.redacted_because.is_some());
    if redacted {
        return Ok(None);
    }
    Ok(Some(parsed))
}

async fn get_state_content<C: DeserializeOwned>(
    room: &Room,
    unstable: &str,
    stable: &str,
) -> Result<Option<C>> {
    let chosen = match (
        read_state(room, unstable).await?,
        read_state(room, stable).await?,
    ) {
        (Some(u), Some(s)) if s.origin_server_ts > u.origin_server_ts => s,
        (Some(u), _) => u,
        (None, Some(s)) => s,
        (None, None) => return Ok(None),
    };
    Ok(Some(serde_json::from_value(chosen.content)?))
}

pub mod endpoint {
    //! The client-server availability query endpoint (MSC4496 §4.5), declared with ruma's
    //! [`metadata!`](matrix_sdk::ruma::api::metadata) macro so it can be sent with
    //! [`Client::send`](matrix_sdk::Client::send).
    //!
    //! `POST /_matrix/client/unstable/org.matrix.msc4496/user/{userId}/availability`
    //!
    //! Only the unstable path is declared. The request body is an [`AvailabilityRequest`] and
    //! the response body an [`AvailabilityResponse`].
    //!
    //! The `#[request]`/`#[response]` attribute macros cannot be used here because they require
    //! the calling crate to define `client` and `server` Cargo features; the traits are
    //! implemented by hand instead, equivalently to what the macros generate.

    use matrix_sdk::ruma::{
        OwnedUserId,
        api::{
            IncomingResponse, Metadata, OutgoingBody, OutgoingRequest,
            auth_scheme::AccessToken,
            error::{DeserializationError, IntoHttpError},
            metadata,
            path_builder::PathBuilder,
        },
        exports::{bytes::BufMut, http},
        http_headers::APPLICATION_JSON,
        serde::json_to_buf,
    };

    use crate::events::{AvailabilityRequest, AvailabilityResponse};

    /// The unstable feature flag of MSC4496 (as advertised in `/versions`).
    pub const UNSTABLE_FEATURE: &str = "org.matrix.msc4496";

    metadata! {
        method: POST,
        rate_limited: true,
        authentication: AccessToken,
        history: {
            unstable("org.matrix.msc4496") => "/_matrix/client/unstable/org.matrix.msc4496/user/{user_id}/availability",
        }
    }

    /// Request type for the availability query endpoint.
    #[derive(Clone, Debug)]
    pub struct Request {
        /// The user whose availability is queried.
        pub user_id: OwnedUserId,
        /// The request body (the queried range).
        pub body: AvailabilityRequest,
    }

    impl Request {
        /// Creates a request for the availability of `user_id` in `body`'s range.
        pub fn new(user_id: OwnedUserId, body: AvailabilityRequest) -> Self {
            Self { user_id, body }
        }
    }

    impl OutgoingRequest for Request {
        type Body = AvailabilityRequest;
        type EndpointError = matrix_sdk::ruma::api::error::Error;
        type IncomingResponse = AvailabilityResponse;

        fn try_into_http_request_inner(
            self,
            base_url: &str,
            path_builder_input: <<Self as Metadata>::PathBuilder as PathBuilder>::Input<'_>,
        ) -> Result<http::Request<Self::Body>, IntoHttpError> {
            let url = Self::make_endpoint_url(path_builder_input, base_url, &[&self.user_id], "")?;
            Ok(http::Request::builder()
                .method(Self::METHOD)
                .uri(url)
                .body(self.body)?)
        }
    }

    impl OutgoingBody for AvailabilityRequest {
        type Error = serde_json::Error;

        fn content_type(&self) -> Option<http::HeaderValue> {
            Some(APPLICATION_JSON)
        }

        fn try_into_buf<T: Default + BufMut + AsRef<[u8]>>(self) -> serde_json::Result<T> {
            json_to_buf(&self)
        }
    }

    impl IncomingResponse for AvailabilityResponse {
        type EndpointError = matrix_sdk::ruma::api::error::Error;

        fn try_from_http_response_inner(
            response: http::Response<&[u8]>,
        ) -> Result<Self, DeserializationError> {
            Ok(serde_json::from_slice(response.body())?)
        }
    }
}

/// Queries the availability of `user_id` in `[range_start, range_end]` with the client-server
/// endpoint of MSC4496 §4.5 (unstable path
/// `/_matrix/client/unstable/org.matrix.msc4496/user/{userId}/availability`).
///
/// # Errors
///
/// A `403 M_FORBIDDEN` (the caller has Tier 0) is returned as [`Error::PermissionDenied`].
/// Homeservers without MSC4496 support typically answer `404 M_UNRECOGNIZED`, which is
/// returned as [`Error::Http`].
pub async fn query_availability(
    client: &Client,
    user_id: &UserId,
    range_start: DateTime<Utc>,
    range_end: DateTime<Utc>,
) -> Result<AvailabilityResponse> {
    if range_end < range_start {
        return Err(Error::InconsistentDateTime(
            "range_end is before range_start".into(),
        ));
    }
    let request = endpoint::Request::new(
        user_id.to_owned(),
        AvailabilityRequest {
            range_start,
            range_end,
        },
    );
    match client.send(request).await {
        Ok(response) => Ok(response),
        Err(err) => {
            if let Some(api_error) = err.as_client_api_error()
                && matches!(api_error.error_kind(), Some(ErrorKind::Forbidden))
            {
                let message = match &api_error.body {
                    matrix_sdk::ruma::api::error::ErrorBody::Standard(body) => body.message.clone(),
                    _ => String::new(),
                };
                warn!(%user_id, "availability query forbidden: {message}");
                return Err(Error::PermissionDenied(format!(
                    "availability of {user_id} is not shared with this user: {message}"
                )));
            }
            Err(err.into())
        }
    }
}
