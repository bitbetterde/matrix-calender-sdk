//! Invites and RSVPs (MSC4496 §3).
//!
//! The organiser sends a [`calendar_invite`](CalendarInviteEventContent) into a room shared with
//! the invitee(s) (a DM or a group room) with [`send_invite`]. Invitees answer with a
//! [`calendar_rsvp`](CalendarRsvpEventContent) that carries an `m.reference` relation to the
//! invite ([`send_rsvp`], or the convenience [`respond`]). Revisions increase the invite's
//! `sequence`; [`cancel_invite`] sends a `CANCEL` and [`counter_propose`] a `COUNTER`.
//!
//! [`SchedulingIndex`] aggregates the invites and responses seen in a room timeline into an
//! attendee list per event. It reads raw events, so it accepts both the stable
//! (`m.calendar.invite` / `m.calendar.rsvp`) and the unstable (`org.matrix.msc4496.*`) event
//! type names, and works with decrypted events from encrypted rooms.
//!
//! # Organisers and keys
//!
//! UIDs are only unique per organiser (MSC4496, "Security considerations"), so all scheduling
//! state is keyed by [`EventKey`] `(organiser, uid)`. The organiser of an invite is the
//! `calendar_event.organizer` it names, or its sender if it names none. Only the organiser may
//! send `REQUEST`s and `CANCEL`s: an invite naming someone else as organiser is ignored, so
//! another room member cannot hijack or cancel an event by re-using its `uid`. A `COUNTER`
//! comes from an invitee and must name the organiser it is addressed to.

use std::collections::{BTreeMap, HashMap, HashSet};

use matrix_sdk::{
    Room,
    deserialized_responses::TimelineEvent,
    room::MessagesOptions,
    ruma::{
        EventId, MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedUserId, UInt, UserId,
        api::client::filter::RoomEventFilter, events::AnySyncTimelineEvent, serde::Raw,
    },
};
use serde::{Deserialize, Serialize, de::IgnoredAny};
use tracing::{debug, warn};

use crate::{
    Error, Result,
    events::{
        CalendarInvite, CalendarInviteEventContent, CalendarRsvp, CalendarRsvpEventContent,
        InviteEventSummary, InviteMethod, PartStat,
    },
    ids,
};

pub use crate::key::EventKey;

/// Sends `invite` as a `calendar_invite` event into `room` (any room shared with the
/// invitee(s), for example a DM) and returns the event ID of the invite.
///
/// The `m.text` fallback is generated from the invite.
///
/// # Errors
///
/// Returns [`Error::PermissionDenied`] if the invite is not a `COUNTER` and names an organiser
/// other than the current user: only the organiser may send `REQUEST`s and `CANCEL`s, and
/// [`SchedulingIndex`] ignores such invites, so sending one would only confuse other clients.
pub async fn send_invite(room: &Room, invite: CalendarInvite) -> Result<OwnedEventId> {
    if invite.uid.is_empty() {
        return Err(Error::InvalidIdentifier("invite has an empty uid".into()));
    }
    if invite.method != InviteMethod::Counter
        && let Some(organizer) = &invite.calendar_event.organizer
        && organizer != room.own_user_id()
    {
        return Err(Error::PermissionDenied(format!(
            "a {} invite naming {organizer} as organiser can only be sent by {organizer}, not by \
             {}",
            invite.method,
            room.own_user_id()
        )));
    }
    let content = CalendarInviteEventContent::new(invite);
    Ok(room.send(content).await?.response.event_id)
}

/// Sends `rsvp` as a `calendar_rsvp` event with an `m.reference` relation to the invite
/// `invite_event_id` and returns the event ID of the RSVP.
///
/// The `m.text` fallback is generic (e.g. `Accepted the invitation`); use [`respond`] to get a
/// fallback naming the event.
pub async fn send_rsvp(
    room: &Room,
    invite_event_id: &EventId,
    rsvp: CalendarRsvp,
) -> Result<OwnedEventId> {
    let content = CalendarRsvpEventContent::new(invite_event_id.to_owned(), rsvp);
    Ok(room.send(content).await?.response.event_id)
}

/// Responds to the invite `invite_event_id` in `room` with `partstat` and an optional comment.
///
/// The invite is fetched from the homeserver (and decrypted if necessary) to copy its `uid`
/// and `sequence` and to build an `m.text` fallback such as
/// `Accepted: Team standup on 2026-07-01`.
///
/// # Errors
///
/// Returns [`Error::InvalidIdentifier`] if the event is not a calendar invite (or has been
/// redacted or cannot be decrypted), [`Error::Other`] if the invite is not a `REQUEST`
/// (cancellations and counter-proposals are not answered with an RSVP), and
/// [`Error::PermissionDenied`] if the invite names an organiser other than its sender (a
/// spoofed invite that [`SchedulingIndex`] ignores as well).
pub async fn respond(
    room: &Room,
    invite_event_id: &EventId,
    partstat: PartStat,
    comment: Option<String>,
) -> Result<OwnedEventId> {
    let event = room.event(invite_event_id, None).await?;
    let parsed: RawEvent = event.raw().deserialize_as_unchecked()?;
    if !ids::is_calendar_invite_type(&parsed.event_type) {
        return Err(Error::InvalidIdentifier(format!(
            "event {invite_event_id} is not a calendar invite (type {})",
            parsed.event_type
        )));
    }
    if parsed.is_redacted() {
        return Err(Error::InvalidIdentifier(format!(
            "invite {invite_event_id} has been redacted"
        )));
    }
    let sender = parsed.sender.clone().ok_or_else(|| {
        Error::InvalidIdentifier(format!("invite {invite_event_id} has no sender"))
    })?;
    let content: CalendarInviteEventContent = serde_json::from_value(parsed.content)?;
    let invite = content.invite;
    if invite.method != InviteMethod::Request {
        return Err(Error::Other(format!(
            "cannot respond to a {} invite",
            invite.method
        )));
    }
    if let Some(organizer) = &invite.calendar_event.organizer
        && *organizer != sender
    {
        return Err(Error::PermissionDenied(format!(
            "invite {invite_event_id} was sent by {sender} but names {organizer} as organiser"
        )));
    }

    let mut rsvp = CalendarRsvp::for_invite(&invite, partstat);
    rsvp.comment = comment;
    let content = CalendarRsvpEventContent::with_summary(
        invite_event_id.to_owned(),
        rsvp,
        &invite.calendar_event,
    );
    Ok(room.send(content).await?.response.event_id)
}

/// Cancels the event described by `previous` (the latest invite sent for it) by sending a
/// `CANCEL` invite with the same `uid` and `calendar_event` and `sequence + 1`.
///
/// Fails with [`Error::PermissionDenied`] if `previous` names another user as organiser (see
/// [`send_invite`]).
pub async fn cancel_invite(room: &Room, previous: &CalendarInvite) -> Result<OwnedEventId> {
    let sequence = previous
        .sequence
        .checked_add(1)
        .ok_or_else(|| Error::Other("invite sequence overflow".into()))?;
    let invite = CalendarInvite {
        uid: previous.uid.clone(),
        sequence,
        method: InviteMethod::Cancel,
        calendar_event: previous.calendar_event.clone(),
    };
    send_invite(room, invite).await
}

/// Sends a `COUNTER` proposal (an invitee proposing a change) for the invite `previous`.
///
/// The counter carries the same `uid` and `sequence` as `previous` and `proposed` as the
/// event summary. As in iTIP, the organiser cannot be changed by a counter-proposal:
/// `proposed.organizer` is set to the [organiser](InviteRecord::organizer) of `previous`
/// (explicitly, so that the counter can be attributed even though an invitee sends it).
pub async fn counter_propose(
    room: &Room,
    previous: &InviteRecord,
    mut proposed: InviteEventSummary,
) -> Result<OwnedEventId> {
    proposed.organizer = Some(previous.organizer().to_owned());
    let invite = CalendarInvite {
        uid: previous.invite.uid.clone(),
        sequence: previous.invite.sequence,
        method: InviteMethod::Counter,
        calendar_event: proposed,
    };
    send_invite(room, invite).await
}

/// An invite (`REQUEST`, `CANCEL` or `COUNTER`) seen in the room timeline.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InviteRecord {
    /// The event ID of the invite.
    pub event_id: OwnedEventId,
    /// The sender of the invite (the organiser for `REQUEST`/`CANCEL`, an invitee for
    /// `COUNTER`).
    pub sender: OwnedUserId,
    /// The server timestamp of the invite.
    pub origin_server_ts: MilliSecondsSinceUnixEpoch,
    /// The invite itself.
    pub invite: CalendarInvite,
}

impl InviteRecord {
    /// The organiser of the scheduled event: the `calendar_event.organizer` the invite names,
    /// or its sender if it names none.
    pub fn organizer(&self) -> &UserId {
        self.invite
            .calendar_event
            .organizer
            .as_deref()
            .unwrap_or(&self.sender)
    }

    /// The `(organiser, uid)` key the invite is filed under in a [`SchedulingIndex`].
    pub fn key(&self) -> EventKey {
        EventKey::new(self.organizer().to_owned(), self.invite.uid.clone())
    }
}

/// An RSVP seen in the room timeline, resolved against the invite it references.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RsvpRecord {
    /// The event ID of the RSVP.
    pub event_id: OwnedEventId,
    /// The responding user (the RSVP's sender).
    pub user: OwnedUserId,
    /// The server timestamp of the RSVP.
    pub origin_server_ts: MilliSecondsSinceUnixEpoch,
    /// The response itself.
    pub rsvp: CalendarRsvp,
    /// The event ID of the invite this RSVP references.
    pub invite_event_id: OwnedEventId,
}

/// The current participation status of one user for one event.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttendeeStatus {
    /// The user.
    pub user: OwnedUserId,
    /// The participation status of the user's newest response.
    pub partstat: PartStat,
    /// The invite `sequence` the newest response answered.
    pub sequence: u32,
    /// The comment of the newest response, if any.
    pub comment: Option<String>,
    /// When the newest response was sent.
    pub responded_at: MilliSecondsSinceUnixEpoch,
    /// Whether the response answered an older revision (a lower `sequence`) than the latest
    /// invite, so the user may want to be asked again.
    pub stale: bool,
}

#[derive(Clone, Debug, Default)]
struct KeyState {
    invites: Vec<InviteRecord>,
    responses: Vec<RsvpRecord>,
}

/// Aggregated scheduling state (invites and RSVPs) for the events seen in one room, built from
/// raw timeline events and keyed by [`EventKey`] `(organiser, uid)`.
///
/// Feed events with [`SchedulingIndex::apply_raw_sync`] or
/// [`SchedulingIndex::apply_timeline_event`] in any order (sync delivers older events first,
/// backward pagination newer ones first), or build an index from the room history with
/// [`SchedulingIndex::load`].
///
/// Rules:
///
/// - Both the stable and the unstable event type names are accepted.
/// - An invite's organiser is the `calendar_event.organizer` it names, or its sender if it
///   names none ([`InviteRecord::organizer`]). `REQUEST`s, `CANCEL`s and invites with unknown
///   methods are only accepted from their organiser; invites naming another user as
///   organiser are ignored with a warning, so that other room members cannot hijack or cancel
///   an event by re-using its `uid`.
/// - A `COUNTER` (from an invitee) must name the organiser and is filed under
///   `(organiser, uid)`; counters without an organiser are ignored.
/// - An RSVP must reference its invite with an `m.reference` relation and is filed under the
///   key of that invite. If the invite is not known yet, the RSVP is buffered and resolved when
///   the invite arrives; RSVPs referencing an ignored or redacted invite are dropped.
/// - An RSVP whose `uid` differs from the referenced invite's `uid` is ignored.
/// - Redacted events, and events redacted later by an `m.room.redaction` fed into the index,
///   are dropped. Redacting an invite also drops the RSVPs that reference it. Events that fail
///   to deserialize are skipped with a warning.
/// - Every event ID is applied at most once.
#[derive(Clone, Debug, Default)]
pub struct SchedulingIndex {
    by_key: BTreeMap<EventKey, KeyState>,
    /// Invite event ID → key.
    invite_keys: HashMap<OwnedEventId, EventKey>,
    /// RSVPs whose invite has not been seen yet, keyed by the invite event ID.
    pending: HashMap<OwnedEventId, Vec<RsvpRecord>>,
    /// Every event ID that has been applied, ignored as unauthorised, or redacted.
    seen: HashSet<OwnedEventId>,
}

/// The fields of a raw timeline event the index needs.
#[derive(Deserialize)]
struct RawEvent {
    #[serde(rename = "type")]
    event_type: String,
    event_id: Option<OwnedEventId>,
    sender: Option<OwnedUserId>,
    origin_server_ts: Option<MilliSecondsSinceUnixEpoch>,
    #[serde(default)]
    content: serde_json::Value,
    #[serde(default)]
    unsigned: Option<RawUnsigned>,
    /// `redacts` of an `m.room.redaction` in room versions before v11.
    redacts: Option<OwnedEventId>,
}

#[derive(Deserialize)]
struct RawUnsigned {
    redacted_because: Option<IgnoredAny>,
}

impl RawEvent {
    fn is_redacted(&self) -> bool {
        self.unsigned
            .as_ref()
            .is_some_and(|u| u.redacted_because.is_some())
    }

    /// The redacted event ID of an `m.room.redaction` (top-level `redacts` or, from room
    /// version 11, `content.redacts`).
    fn redaction_target(&self) -> Option<OwnedEventId> {
        self.redacts.clone().or_else(|| {
            self.content
                .get("redacts")
                .and_then(|v| v.as_str())
                .and_then(|s| OwnedEventId::try_from(s).ok())
        })
    }
}

impl SchedulingIndex {
    /// Creates an empty index.
    pub fn new() -> Self {
        Self::default()
    }

    /// Applies a raw timeline event (from sync, pagination, or a decrypted event).
    ///
    /// Returns `Ok(true)` if the index changed (including an RSVP being buffered until its
    /// invite arrives). Events that are not calendar invites, RSVPs or redactions, events that
    /// have already been applied, redacted events, invites from someone other than their
    /// organiser and malformed events are ignored (the latter two with a warning) and yield
    /// `Ok(false)`; malformed input is never an error.
    pub fn apply_raw_sync(&mut self, raw: &Raw<AnySyncTimelineEvent>) -> Result<bool> {
        let ev: RawEvent = match raw.deserialize_as_unchecked() {
            Ok(ev) => ev,
            Err(e) => {
                warn!("skipping malformed timeline event: {e}");
                return Ok(false);
            }
        };
        Ok(self.apply(ev, None))
    }

    /// Applies a [`TimelineEvent`] as returned by `Room::messages`, `Room::event` or the event
    /// cache. Decrypted events are read in their decrypted form; events that could not be
    /// decrypted are ignored.
    pub fn apply_timeline_event(&mut self, ev: &TimelineEvent) -> Result<bool> {
        let parsed: RawEvent = match ev.raw().deserialize_as_unchecked() {
            Ok(parsed) => parsed,
            Err(e) => {
                warn!("skipping malformed timeline event: {e}");
                return Ok(false);
            }
        };
        Ok(self.apply(parsed, ev.timestamp()))
    }

    fn apply(&mut self, ev: RawEvent, fallback_ts: Option<MilliSecondsSinceUnixEpoch>) -> bool {
        let is_invite = ids::is_calendar_invite_type(&ev.event_type);
        let is_rsvp = ids::is_calendar_rsvp_type(&ev.event_type);
        let is_redaction = ev.event_type == "m.room.redaction";
        if !is_invite && !is_rsvp && !is_redaction {
            return false;
        }

        let Some(event_id) = ev.event_id.clone() else {
            warn!(
                event_type = ev.event_type,
                "skipping event without event_id"
            );
            return false;
        };
        if self.seen.contains(&event_id) {
            return false;
        }

        if is_redaction {
            return match ev.redaction_target() {
                Some(target) => {
                    self.seen.insert(event_id);
                    self.remove_event(target)
                }
                None => false,
            };
        }

        if ev.is_redacted() {
            debug!(%event_id, "dropping redacted scheduling event");
            self.seen.insert(event_id.clone());
            // RSVPs buffered for a redacted invite will never resolve.
            self.pending.remove(&event_id);
            return false;
        }

        let (Some(sender), Some(ts)) = (ev.sender.clone(), ev.origin_server_ts.or(fallback_ts))
        else {
            warn!(%event_id, "skipping scheduling event without sender or origin_server_ts");
            return false;
        };

        let changed = if is_invite {
            match serde_json::from_value::<CalendarInviteEventContent>(ev.content) {
                Ok(content) => self.insert_invite(InviteRecord {
                    event_id: event_id.clone(),
                    sender,
                    origin_server_ts: ts,
                    invite: content.invite,
                }),
                Err(e) => {
                    warn!(%event_id, "skipping malformed calendar invite: {e}");
                    return false;
                }
            }
        } else {
            // `Reference` does not check the tag on input, so check the relation type here.
            let rel_type = ev
                .content
                .get("m.relates_to")
                .and_then(|r| r.get("rel_type"))
                .and_then(|t| t.as_str());
            if rel_type != Some("m.reference") {
                warn!(%event_id, ?rel_type, "skipping calendar RSVP without m.reference relation");
                return false;
            }
            match serde_json::from_value::<CalendarRsvpEventContent>(ev.content) {
                Ok(content) => {
                    let invite_event_id = content.relates_to.event_id;
                    self.insert_response(RsvpRecord {
                        event_id: event_id.clone(),
                        user: sender,
                        origin_server_ts: ts,
                        rsvp: content.rsvp,
                        invite_event_id,
                    })
                }
                Err(e) => {
                    warn!(%event_id, "skipping malformed calendar RSVP: {e}");
                    return false;
                }
            }
        };
        self.seen.insert(event_id);
        changed
    }

    /// Whether `record` may be filed under its key (see the rules on [`SchedulingIndex`]).
    fn is_authorised(record: &InviteRecord) -> bool {
        let named = record.invite.calendar_event.organizer.as_deref();
        if record.invite.method == InviteMethod::Counter {
            if named.is_none() {
                warn!(
                    invite = %record.event_id,
                    sender = %record.sender,
                    "ignoring COUNTER that does not name the organiser"
                );
                return false;
            }
            return true;
        }
        match named {
            Some(organizer) if organizer != &*record.sender => {
                warn!(
                    invite = %record.event_id,
                    sender = %record.sender,
                    %organizer,
                    method = %record.invite.method,
                    "ignoring invite that was not sent by the organiser it names"
                );
                false
            }
            _ => true,
        }
    }

    fn insert_invite(&mut self, record: InviteRecord) -> bool {
        let event_id = record.event_id.clone();
        if !Self::is_authorised(&record) {
            // RSVPs to an ignored invite are dropped as well.
            self.pending.remove(&event_id);
            return false;
        }
        let key = record.key();
        self.invite_keys.insert(event_id.clone(), key.clone());
        self.by_key
            .entry(key.clone())
            .or_default()
            .invites
            .push(record);

        for response in self.pending.remove(&event_id).unwrap_or_default() {
            self.attach_response(&key, response);
        }
        true
    }

    fn insert_response(&mut self, response: RsvpRecord) -> bool {
        match self.invite_keys.get(&response.invite_event_id).cloned() {
            Some(key) => self.attach_response(&key, response),
            // The referenced event was redacted, ignored, or is not an invite.
            None if self.seen.contains(&response.invite_event_id) => {
                debug!(
                    rsvp = %response.event_id,
                    invite = %response.invite_event_id,
                    "dropping RSVP to an unknown, ignored or redacted invite"
                );
                false
            }
            None => {
                self.pending
                    .entry(response.invite_event_id.clone())
                    .or_default()
                    .push(response);
                true
            }
        }
    }

    fn attach_response(&mut self, key: &EventKey, response: RsvpRecord) -> bool {
        if response.rsvp.uid != key.uid {
            warn!(
                rsvp = %response.event_id,
                invite = %response.invite_event_id,
                rsvp_uid = response.rsvp.uid,
                invite_uid = key.uid,
                "ignoring RSVP whose uid does not match the referenced invite"
            );
            return false;
        }
        self.by_key
            .entry(key.clone())
            .or_default()
            .responses
            .push(response);
        true
    }

    /// Removes the invite or response `target` (after a redaction), together with the RSVPs
    /// referencing a removed invite. Returns whether anything visible was removed.
    fn remove_event(&mut self, target: OwnedEventId) -> bool {
        // Remember the target so that it is dropped if it is only seen later (backward
        // pagination delivers the redaction before the redacted event).
        self.seen.insert(target.clone());
        // RSVPs buffered for the target (if it is an invite) will never resolve.
        self.pending.remove(&target);
        let mut removed = false;
        if let Some(key) = self.invite_keys.remove(&target)
            && let Some(state) = self.by_key.get_mut(&key)
        {
            state.invites.retain(|r| r.event_id != target);
            state.responses.retain(|r| r.invite_event_id != target);
            removed = true;
        }
        for state in self.by_key.values_mut() {
            let before = state.responses.len();
            state.responses.retain(|r| r.event_id != target);
            removed |= state.responses.len() != before;
        }
        for responses in self.pending.values_mut() {
            responses.retain(|r| r.event_id != target);
        }
        self.pending.retain(|_, v| !v.is_empty());
        self.by_key
            .retain(|_, s| !s.invites.is_empty() || !s.responses.is_empty());
        removed
    }

    /// All invites (including cancellations and counter-proposals) for `key`, sorted by
    /// `sequence`, then by timestamp.
    pub fn invites(&self, key: &EventKey) -> Vec<&InviteRecord> {
        let mut invites: Vec<_> = self
            .by_key
            .get(key)
            .map(|s| s.invites.iter().collect())
            .unwrap_or_default();
        invites.sort_by_key(|r| (r.invite.sequence, r.origin_server_ts));
        invites
    }

    /// The latest organiser revision for `key`: the `REQUEST` or `CANCEL` with the highest
    /// `sequence` (ties: the latest timestamp).
    ///
    /// `COUNTER` proposals (and invites with unknown methods) are not organiser revisions and
    /// are ignored, unless no `REQUEST` or `CANCEL` exists for `key`.
    pub fn latest_invite(&self, key: &EventKey) -> Option<&InviteRecord> {
        let state = self.by_key.get(key)?;
        let order = |r: &&InviteRecord| (r.invite.sequence, r.origin_server_ts);
        state
            .invites
            .iter()
            .filter(|r| {
                matches!(
                    r.invite.method,
                    InviteMethod::Request | InviteMethod::Cancel
                )
            })
            .max_by_key(order)
            .or_else(|| state.invites.iter().max_by_key(order))
    }

    /// All resolved responses for `key`, sorted by timestamp.
    ///
    /// RSVPs whose invite has not been seen yet, or has been redacted, are not included.
    pub fn responses(&self, key: &EventKey) -> Vec<&RsvpRecord> {
        let mut responses: Vec<_> = self
            .by_key
            .get(key)
            .map(|s| s.responses.iter().collect())
            .unwrap_or_default();
        responses.sort_by_key(|r| r.origin_server_ts);
        responses
    }

    /// The attendee list for `key`: one entry per responding user, built from that user's
    /// newest response (by timestamp; ties: the higher `sequence`), sorted by user ID.
    ///
    /// An entry is [`stale`](AttendeeStatus::stale) if its `sequence` is lower than the
    /// [latest invite](Self::latest_invite)'s.
    pub fn attendees(&self, key: &EventKey) -> Vec<AttendeeStatus> {
        let latest_sequence = self.latest_invite(key).map(|r| r.invite.sequence);
        let mut newest: BTreeMap<&OwnedUserId, &RsvpRecord> = BTreeMap::new();
        for response in self
            .by_key
            .get(key)
            .map(|s| s.responses.iter())
            .into_iter()
            .flatten()
        {
            let order = |r: &RsvpRecord| (r.origin_server_ts, r.rsvp.sequence);
            match newest.get(&response.user) {
                Some(current) if order(current) >= order(response) => {}
                _ => {
                    newest.insert(&response.user, response);
                }
            }
        }
        newest
            .into_values()
            .map(|r| AttendeeStatus {
                user: r.user.clone(),
                partstat: r.rsvp.partstat.clone(),
                sequence: r.rsvp.sequence,
                comment: r.rsvp.comment.clone(),
                responded_at: r.origin_server_ts,
                stale: latest_sequence.is_some_and(|s| r.rsvp.sequence < s),
            })
            .collect()
    }

    /// Whether the [latest invite](Self::latest_invite) for `key` is a `CANCEL`.
    pub fn is_cancelled(&self, key: &EventKey) -> bool {
        self.latest_invite(key)
            .is_some_and(|r| r.invite.method == InviteMethod::Cancel)
    }

    /// The `(organiser, uid)` keys with at least one invite or resolved response, in order.
    pub fn keys(&self) -> impl Iterator<Item = &EventKey> {
        self.by_key.keys()
    }

    /// The number of RSVPs whose referenced invite has not been seen yet.
    pub fn unresolved_responses(&self) -> usize {
        self.pending.values().map(Vec::len).sum()
    }

    /// Builds an index from the history of `room`, paginating backwards from the end of the
    /// timeline with `/messages`.
    ///
    /// Stops at the start of the (visible) history or once `max_events` events have been
    /// read, if given. Only invite, RSVP, redaction and encrypted events are requested; events
    /// are decrypted by the SDK where possible.
    pub async fn load(room: &Room, max_events: Option<usize>) -> Result<Self> {
        const PAGE_SIZE: usize = 100;

        let mut index = Self::new();
        let mut from: Option<String> = None;
        let mut read = 0usize;

        loop {
            let remaining = max_events.map_or(PAGE_SIZE, |m| m.saturating_sub(read));
            if remaining == 0 {
                break;
            }
            let page = remaining.min(PAGE_SIZE);

            let mut options = MessagesOptions::backward().from(from.as_deref());
            // `page` is at most `PAGE_SIZE`, so the cast cannot truncate.
            options.limit = UInt::from(page as u32);
            options.filter = scheduling_filter();

            let messages = room.messages(options).await?;
            read += messages.chunk.len();
            for ev in &messages.chunk {
                index.apply_timeline_event(ev)?;
            }

            match messages.end {
                // With a type filter the server may return an empty chunk but more history.
                Some(end) if Some(&end) != from.as_ref() => {
                    from = Some(end);
                }
                _ => break,
            }
        }
        Ok(index)
    }
}

fn scheduling_filter() -> RoomEventFilter {
    let mut filter = RoomEventFilter::default();
    filter.types = Some(
        [
            ids::CALENDAR_INVITE,
            ids::CALENDAR_INVITE_UNSTABLE,
            ids::CALENDAR_RSVP,
            ids::CALENDAR_RSVP_UNSTABLE,
            "m.room.redaction",
            "m.room.encrypted",
        ]
        .into_iter()
        .map(ToOwned::to_owned)
        .collect(),
    );
    filter
}
