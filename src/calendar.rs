//! High-level calendar room API (MSC4496 §1, §2).
//!
//! - [`Calendar`] wraps a [`matrix_sdk::Room`] whose room type is a calendar room type. It
//!   creates calendar rooms, reads and writes the `calendar_info` state event, and sends calendar
//!   events, edits, cancellations, instance overrides and redactions.
//! - [`CalendarInvitation`] is a pending invitation to a calendar room, i.e. a calendar
//!   somebody shared with the current user: [`list_calendar_invitations`] lists them,
//!   [`CalendarInvitation::accept`] joins the room and [`CalendarInvitation::decline`] rejects
//!   the invite. (This is room membership; invitations to a single calendar *event* are
//!   [`scheduling`](crate::scheduling).)
//! - [`CalendarIndex`] is an in-memory view of the calendar events in a room, built from raw
//!   timeline events. It applies `m.replace` edits (newest valid edit wins, only by the original
//!   sender), attaches instance overrides to their series, and handles redactions. Events are
//!   keyed by [`EventKey`] (`sender`, `uid`) because UIDs are only unique per sender
//!   (MSC4496, "Security considerations").
//!
//! # Stable and unstable identifiers
//!
//! The SDK sends the unstable identifiers. ruma's `alias` attribute does not make typed
//! matrix-sdk lookups match the stable names, so everything in this module that *reads*
//! events works on raw JSON: the event `type` is checked with
//! [`ids::is_calendar_event_type`] (or the room/info equivalents) and the `content` is
//! deserialized by hand. This accepts both `org.matrix.msc4496.*` and `m.calendar.*`.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::{Arc, Mutex, PoisonError},
};

use chrono::{DateTime, TimeDelta, Utc};
use chrono_tz::Tz;
use matrix_sdk::{
    Client, Room, RoomState,
    deserialized_responses::{RawAnySyncOrStrippedState, TimelineEvent},
    event_handler::EventHandlerDropGuard,
    room::MessagesOptions,
    ruma::{
        EventId, MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedUserId, RoomId, RoomVersionId,
        UInt, UserId,
        api::client::{
            filter::RoomEventFilter,
            room::{
                Visibility,
                create_room::v3::{CreationContent, Request as CreateRoomRequest, RoomPreset},
            },
        },
        events::{
            AnyInitialStateEvent, AnySyncTimelineEvent, AnyTimelineEvent, InitialStateEvent,
            StateEventType, room::encryption::RoomEncryptionEventContent,
        },
        room::RoomType,
        serde::Raw,
    },
};
use serde::{Deserialize, Serialize, de::Error as _};
use serde_json::value::RawValue as RawJsonValue;
use tokio::sync::{RwLock, RwLockReadGuard, broadcast};
use tracing::{debug, warn};

use crate::{
    Error, Result, availability,
    events::{
        BusyInterval, CalendarEvent, CalendarEventEventContent,
        CalendarEventEventContentWithoutRelation, CalendarInfo, CalendarInfoEventContent, Status,
    },
    ids,
    permissions::{
        self, CalendarMember, CalendarPermissions, CalendarPermissionsSummary, CalendarRole,
    },
    recurrence::{self, ExpandOptions, Occurrence},
};

/// The `m.room.redaction` event type.
const REDACTION: &str = "m.room.redaction";
/// The `m.room.encrypted` event type (kept in the `/messages` filter so E2EE rooms work).
const ENCRYPTED: &str = "m.room.encrypted";
/// Default `/messages` page size used by [`Calendar::load`].
const DEFAULT_PAGE_SIZE: u32 = 100;
/// Capacity of the [`LiveCalendar::subscribe`] broadcast channel.
const CHANGE_CHANNEL_CAPACITY: usize = 256;
/// Maximum number of redacted event IDs a [`CalendarIndex`] remembers.
pub const MAX_REMEMBERED_REDACTIONS: usize = 10_000;

// ---------------------------------------------------------------------------------------------
// Calendar room
// ---------------------------------------------------------------------------------------------

/// Options for [`Calendar::create`].
#[derive(Clone, Debug)]
pub struct CreateCalendarOptions {
    /// The room name, also used as the `m.text` fallback of the `calendar_info` state event.
    pub name: String,
    /// Calendar metadata (colour, home timezone) for the `calendar_info` state event.
    pub info: CalendarInfo,
    /// Whether to enable end-to-end encryption (`m.room.encryption` in the initial state).
    ///
    /// Defaults to `false`. Note that encryption hides event contents from the homeserver,
    /// which therefore cannot compute availability from them.
    pub encrypted: bool,
    /// Users to invite when the room is created. They get the
    /// [default role](CalendarPermissions::default_role) (or the owner role with
    /// [`RoomPreset::TrustedPrivateChat`]); use [`members`](Self::members) to give invitees
    /// a specific role.
    pub invite: Vec<OwnedUserId>,
    /// Users to invite when the room is created, each with an explicit role, which is set in
    /// the initial power levels (so it is in place before they can join).
    ///
    /// Explicit roles are pinned: they do not change when the default role changes later.
    pub members: Vec<(OwnedUserId, CalendarRole)>,
    /// The permission scheme applied to the room's initial power levels (see
    /// [`power_levels_for`](crate::permissions::power_levels_for)).
    ///
    /// Defaults to [`CalendarPermissions::default`]: members without an explicit role are
    /// **readers**, who cannot create or change calendar events. Give writers the
    /// [`CalendarRole::Editor`] role through [`members`](Self::members), or set the default
    /// role to `Editor` to let every member write.
    pub permissions: CalendarPermissions,
    /// The room version. `None` (the default) uses the homeserver's default room version,
    /// which is then read from the server's capabilities and pinned in the request when
    /// [`members`](Self::members) need explicit roles (the creator belongs in the `users` map
    /// of the power levels before room version 12, and must not be in it from 12 on).
    pub room_version: Option<RoomVersionId>,
    /// The `m.federate` flag of the `m.room.create` event. Defaults to `true`.
    pub federate: bool,
    /// The room preset. Defaults to [`RoomPreset::PrivateChat`] (invite-only). With
    /// [`RoomPreset::TrustedPrivateChat`] the users in [`invite`](Self::invite) become owners,
    /// like the creator.
    pub preset: RoomPreset,
    /// The room directory visibility. Defaults to [`Visibility::Private`].
    pub visibility: Visibility,
}

impl CreateCalendarOptions {
    /// Creates options for a private, unencrypted, federated calendar named `name` with
    /// default metadata, no invites and the default (read-only by default) permissions.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            info: CalendarInfo::default(),
            encrypted: false,
            invite: Vec::new(),
            members: Vec::new(),
            permissions: CalendarPermissions::default(),
            room_version: None,
            federate: true,
            preset: RoomPreset::PrivateChat,
            visibility: Visibility::Private,
        }
    }
}

impl Default for CreateCalendarOptions {
    fn default() -> Self {
        Self::new("")
    }
}

/// Options for [`Calendar::load_with`].
#[derive(Clone, Debug, Default)]
pub struct LoadOptions {
    /// Stop after this many events (of the filtered types, see [`Calendar::load`]) have been
    /// fed into the index. `None` paginates to the start of the room.
    pub max_events: Option<usize>,
    /// Stop paginating once events older than this timestamp are reached; older events are
    /// not fed into the index.
    ///
    /// Note that a series or event created before `since` will be missing from the index even
    /// if it was edited afterwards: its edits are buffered but never applied.
    pub since: Option<MilliSecondsSinceUnixEpoch>,
    /// The `/messages` page size. Defaults to 100.
    pub page_size: Option<u32>,
}

/// A Matrix room that is a calendar: its `m.room.create` room type is
/// `org.matrix.msc4496.calendar` or `m.calendar`.
#[derive(Clone, Debug)]
pub struct Calendar {
    room: Room,
}

impl Calendar {
    /// Creates a new calendar room.
    ///
    /// The `m.room.create` content gets the unstable calendar room type and the
    /// [`federate`](CreateCalendarOptions::federate) flag; the initial state contains the
    /// `calendar_info` state event (and `m.room.encryption` if
    /// [`encrypted`](CreateCalendarOptions::encrypted) is set). The initial power levels
    /// follow [`CreateCalendarOptions::permissions`] (sent as `power_level_content_override`),
    /// with the roles of [`CreateCalendarOptions::members`] in the `users` map, and both
    /// [`invite`](CreateCalendarOptions::invite) and `members` are invited.
    ///
    /// Note that by default members are **readers**; see [`CreateCalendarOptions::permissions`].
    pub async fn create(client: &Client, opts: CreateCalendarOptions) -> Result<Calendar> {
        opts.permissions.validate()?;
        let info = CalendarInfoEventContent::new(opts.name.clone(), opts.info)?;

        // Explicit roles, and everyone to invite (without duplicates or ourselves).
        let own_user_id = client.user_id().map(ToOwned::to_owned);
        let mut explicit_roles: BTreeMap<OwnedUserId, CalendarRole> = BTreeMap::new();
        let mut invite: Vec<OwnedUserId> = Vec::new();
        let trusted = opts.preset == RoomPreset::TrustedPrivateChat;
        for user in &opts.invite {
            if trusted {
                explicit_roles.insert(user.clone(), CalendarRole::Owner);
            }
            invite.push(user.clone());
        }
        for (user, role) in &opts.members {
            explicit_roles.insert(user.clone(), *role);
            invite.push(user.clone());
        }
        if let Some(own) = &own_user_id {
            explicit_roles.remove(own);
            invite.retain(|user| user != own);
        }
        let mut seen = HashSet::new();
        invite.retain(|user| seen.insert(user.clone()));

        let (power_levels, room_version) = permissions::creation_override(
            client,
            &opts.permissions,
            &explicit_roles,
            opts.room_version,
        )
        .await?;

        let mut creation_content = CreationContent::new();
        creation_content.federate = opts.federate;
        creation_content.room_type = Some(RoomType::from(ids::ROOM_TYPE_CALENDAR_UNSTABLE));

        let mut initial_state: Vec<Raw<AnyInitialStateEvent>> =
            vec![Raw::new(&InitialStateEvent::with_empty_state_key(info))?.cast()];
        if opts.encrypted {
            initial_state.push(
                Raw::new(&InitialStateEvent::with_empty_state_key(
                    RoomEncryptionEventContent::with_recommended_defaults(),
                ))?
                .cast(),
            );
        }

        let mut request = CreateRoomRequest::new();
        request.creation_content = Some(Raw::new(&creation_content)?);
        request.initial_state = initial_state;
        request.name = (!opts.name.is_empty()).then_some(opts.name);
        request.invite = invite;
        request.power_level_content_override = Some(power_levels);
        request.room_version = room_version;
        request.preset = Some(opts.preset);
        request.visibility = opts.visibility;

        let room = client.create_room(request).await?;
        // The create event has not been synced yet, so `try_from_room` would not see the room
        // type; we know it is a calendar because we just created it.
        Ok(Self { room })
    }

    /// Wraps `room` if it is a calendar room.
    ///
    /// Returns [`Error::NotACalendarRoom`] if the room's create event has no room type or one
    /// that is neither the stable nor the unstable calendar room type (including when the
    /// create event has not been synced yet).
    pub fn try_from_room(room: Room) -> Result<Calendar> {
        if is_calendar_room(&room) {
            Ok(Self { room })
        } else {
            Err(Error::NotACalendarRoom(room.room_id().to_owned()))
        }
    }

    /// The underlying room.
    pub fn room(&self) -> &Room {
        &self.room
    }

    /// The room ID.
    pub fn room_id(&self) -> &RoomId {
        self.room.room_id()
    }

    /// Reads the `calendar_info` state event from the local state store, accepting both the
    /// unstable and the stable event type (the unstable one wins if both are present).
    ///
    /// Returns `None` if there is no such state event or its content cannot be parsed.
    pub async fn info(&self) -> Result<Option<CalendarInfoEventContent>> {
        read_calendar_info(&self.room).await
    }

    /// Sends a new `calendar_info` state event (with the unstable type).
    ///
    /// Fails with [`Error::PermissionDenied`] without sending anything if the locally synced
    /// power levels do not allow it (by default only owners may change the calendar info).
    pub async fn set_info(&self, info: CalendarInfoEventContent) -> Result<OwnedEventId> {
        info.info.validate()?;
        permissions::ensure_can_send_state(
            &self.room,
            ids::CALENDAR_INFO_UNSTABLE,
            "change the calendar info",
        )
        .await?;
        Ok(self.room.send_state_event(info).await?.event_id)
    }

    /// Sends a new calendar event.
    ///
    /// `event` is validated first. Instance overrides (events with a `recurrence_id`) must be
    /// sent with [`Calendar::create_override`] instead.
    ///
    /// Fails with [`Error::PermissionDenied`] without sending anything if the locally synced
    /// power levels say that the current user cannot send calendar events (the
    /// [`CalendarRole::Reader`] role). The homeserver enforces this as well.
    pub async fn create_event(&self, event: CalendarEvent) -> Result<OwnedEventId> {
        event.validate()?;
        if event.is_override() {
            return Err(Error::Other(
                "events with a recurrence_id are overrides; use create_override".into(),
            ));
        }
        permissions::ensure_can_write(&self.room, "create calendar events").await?;
        self.send(CalendarEventEventContent::new(event)).await
    }

    /// Edits the calendar event (or override) `original_event_id` with an `m.replace`
    /// relation carrying `event` as the complete new content.
    ///
    /// The original is looked up (from the event cache if enabled, otherwise from the
    /// homeserver) to check that it is a calendar event sent by the current user
    /// ([`Error::PermissionDenied`] otherwise), that it is not itself an edit, and that the
    /// `uid` is unchanged ([`Error::InvalidIdentifier`] otherwise). When the original is an
    /// instance override, `event` must be a valid override with the same `recurrence_id`
    /// (an edit cannot move an override to another instance).
    ///
    /// Matrix only accepts edits from the original sender, so even owners cannot edit other
    /// users' events. Readers get [`Error::PermissionDenied`] before anything is fetched or
    /// sent (see [`Calendar::create_event`]).
    pub async fn update_event(
        &self,
        original_event_id: &EventId,
        event: CalendarEvent,
    ) -> Result<OwnedEventId> {
        event.validate()?;
        permissions::ensure_can_write(&self.room, "edit calendar events").await?;
        let original = self.fetch_own_original(original_event_id).await?;
        if original.event.uid != event.uid {
            return Err(Error::InvalidIdentifier(format!(
                "an edit cannot change the uid ({} -> {})",
                original.event.uid, event.uid
            )));
        }
        if original.event.is_override() {
            event.validate_override()?;
            if event.recurrence_id != original.event.recurrence_id {
                return Err(Error::InvalidIdentifier(
                    "an edit of an override cannot change its recurrence_id".into(),
                ));
            }
        }
        let content =
            CalendarEventEventContent::new(event).into_replacement(original_event_id.to_owned());
        self.send(content).await
    }

    /// Cancels the calendar event `original_event_id`: an edit (see
    /// [`Calendar::update_event`]) of `current` with `status` set to `cancelled`.
    ///
    /// `current` should be the event's current effective content, for example
    /// [`IndexedEvent::content`]. Cancelling is preferred over [`Calendar::delete_event`]
    /// because other clients and bridges can see and propagate it.
    pub async fn cancel_event(
        &self,
        original_event_id: &EventId,
        mut current: CalendarEvent,
    ) -> Result<OwnedEventId> {
        permissions::ensure_can_write(&self.room, "cancel calendar events").await?;
        current.status = Status::Cancelled;
        self.update_event(original_event_id, current).await
    }

    /// Sends an instance override of the recurring series whose original event is
    /// `series_event_id`.
    ///
    /// `instance` must pass [`CalendarEvent::validate_override`] (so it has a `recurrence_id`)
    /// and carry the series' `uid`; the series must have been sent by the current user
    /// ([`Error::PermissionDenied`] otherwise). To cancel a single occurrence, send an override
    /// with status `cancelled`.
    ///
    /// Whether the series is recurring is not checked: only the original event is fetched,
    /// and a series may have become recurring through a later edit. Use
    /// [`Calendar::create_override_for`] to override an occurrence from a [`CalendarIndex`].
    pub async fn create_override(
        &self,
        series_event_id: &EventId,
        instance: CalendarEvent,
    ) -> Result<OwnedEventId> {
        instance.validate_override()?;
        permissions::ensure_can_write(&self.room, "override occurrences of calendar events")
            .await?;
        let series = self.fetch_own_original(series_event_id).await?;
        if series.event.uid != instance.uid {
            return Err(Error::InvalidIdentifier(format!(
                "override uid {} does not match series uid {}",
                instance.uid, series.event.uid
            )));
        }
        self.send(CalendarEventEventContent::override_of(
            series_event_id.to_owned(),
            instance,
        ))
        .await
    }

    /// Redacts `event_id` (an original event, an edit or an override).
    ///
    /// This is destructive: the content is erased for everyone and cannot be bridged as a
    /// cancellation. Prefer [`Calendar::cancel_event`] for calendar semantics.
    pub async fn delete_event(&self, event_id: &EventId, reason: Option<String>) -> Result<()> {
        self.room.redact(event_id, reason.as_deref(), None).await?;
        Ok(())
    }

    /// Builds a [`CalendarIndex`] of the whole room.
    ///
    /// Paginates the room timeline backwards with `/messages`, from the live end to the start
    /// of the room, with a filter for calendar events (both type names), `m.room.redaction`
    /// and `m.room.encrypted` (encrypted events are decrypted by matrix-sdk when possible;
    /// events that cannot be decrypted are skipped).
    ///
    /// This fetches the complete history and can be slow for very long rooms; use
    /// [`Calendar::load_with`] with [`LoadOptions::max_events`] or [`LoadOptions::since`] to
    /// bound it.
    pub async fn load(&self) -> Result<CalendarIndex> {
        self.load_with(LoadOptions::default()).await
    }

    /// Builds a [`CalendarIndex`] like [`Calendar::load`], bounded by `options`.
    pub async fn load_with(&self, options: LoadOptions) -> Result<CalendarIndex> {
        let mut index = CalendarIndex::new();
        self.load_into(&mut index, options).await?;
        Ok(index)
    }

    /// Paginates the room timeline backwards like [`Calendar::load_with`], feeding the events
    /// into an existing `index`. Returns the number of events fed into the index.
    pub async fn load_into(
        &self,
        index: &mut CalendarIndex,
        options: LoadOptions,
    ) -> Result<usize> {
        let page_size = options.page_size.unwrap_or(DEFAULT_PAGE_SIZE).max(1);
        let mut from: Option<String> = None;
        let mut count = 0usize;

        loop {
            let remaining = options.max_events.map(|max| max.saturating_sub(count));
            if remaining == Some(0) {
                break;
            }
            let limit = remaining.map_or(page_size, |r| {
                u32::try_from(r).map_or(page_size, |r| r.min(page_size))
            });

            let mut filter = RoomEventFilter::default();
            filter.types = Some(vec![
                ids::CALENDAR_EVENT_UNSTABLE.to_owned(),
                ids::CALENDAR_EVENT.to_owned(),
                REDACTION.to_owned(),
                ENCRYPTED.to_owned(),
            ]);
            let mut request = MessagesOptions::backward();
            request.from = from.clone();
            request.limit = UInt::from(limit);
            request.filter = filter;

            let messages = self.room.messages(request).await?;
            debug!(
                room_id = %self.room.room_id(),
                events = messages.chunk.len(),
                "paginated calendar room"
            );

            let mut reached_since = false;
            for event in &messages.chunk {
                if let Some(since) = options.since {
                    let ts = event
                        .raw()
                        .get_field::<MilliSecondsSinceUnixEpoch>("origin_server_ts")
                        .ok()
                        .flatten();
                    if ts.is_some_and(|ts| ts < since) {
                        reached_since = true;
                        continue;
                    }
                }
                if let Err(error) = index.apply_timeline_event(event) {
                    warn!(%error, "skipping malformed timeline event");
                }
                count += 1;
                if options.max_events.is_some_and(|max| count >= max) {
                    break;
                }
            }

            // An empty chunk does not mean the start of the room (with a type filter the
            // server may return no matching events but more history); only a missing or
            // repeated `end` token does.
            if reached_since {
                break;
            }
            match messages.end {
                Some(end) if from.as_deref() != Some(end.as_str()) => from = Some(end),
                _ => break,
            }
        }

        Ok(count)
    }

    /// The calendar's home timezone from the `calendar_info` state event, or UTC if it has
    /// none. Used as the fallback timezone for all-day events.
    pub async fn default_timezone(&self) -> Result<Tz> {
        Ok(self
            .info()
            .await?
            .and_then(|info| info.info.timezone)
            .unwrap_or(Tz::UTC))
    }

    /// Loads the whole room (see [`Calendar::load`]) and returns the occurrences in
    /// `[start, end)`, see [`CalendarIndex::occurrences`]. All-day events are placed on the
    /// timeline in [`Calendar::default_timezone`].
    ///
    /// This paginates the full history on every call; keep a [`CalendarIndex`] or a
    /// [`LiveCalendar`] around for repeated queries.
    pub async fn occurrences(
        &self,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Result<Vec<CalendarOccurrence>> {
        let fallback_tz = self.default_timezone().await?;
        let index = self.load().await?;
        index.occurrences(start, end, fallback_tz)
    }

    /// Changes a single occurrence of a recurring series.
    ///
    /// Starts from the occurrence's effective event (the series master, or the override that
    /// already replaces this instance), sets its `start`/`end` to the occurrence's, its
    /// `recurrence_id` to the occurrence's original start and removes the recurrence rule,
    /// then applies `mutate`. If the occurrence is not overridden yet, the result is sent as a
    /// new override with [`Calendar::create_override`]; otherwise the existing override is
    /// edited with [`Calendar::update_event`], so that there is only ever one override per
    /// instance. Returns the ID of the sent event.
    ///
    /// Fails if `occurrence` is not an instance of a recurring series, if `mutate` changes the
    /// `uid` or `recurrence_id`, or for the reasons listed on the methods above (for example
    /// [`Error::PermissionDenied`] if the series was sent by another user).
    pub async fn create_override_for(
        &self,
        occurrence: &CalendarOccurrence,
        mutate: impl FnOnce(&mut CalendarEvent),
    ) -> Result<OwnedEventId> {
        permissions::ensure_can_write(&self.room, "change occurrences of calendar events").await?;
        let occ = &occurrence.occurrence;
        let Some(recurrence_id) = occ.recurrence_id else {
            return Err(Error::Other(format!(
                "{} is not a recurring event; edit it with update_event instead",
                occurrence.event_id
            )));
        };
        let mut event = occ.event.clone();
        event.start = occ.start;
        event.end = occ.end;
        event.recurrence = None;
        event.recurrence_id = Some(recurrence_id);
        mutate(&mut event);
        if event.uid != occ.uid || event.recurrence_id != Some(recurrence_id) {
            return Err(Error::InvalidIdentifier(
                "an override must keep the uid and recurrence_id of its occurrence".into(),
            ));
        }
        match &occurrence.override_event_id {
            Some(override_event_id) => self.update_event(override_event_id, event).await,
            None => self.create_override(&occurrence.event_id, event).await,
        }
    }

    /// Cancels a single occurrence of a recurring series: [`Calendar::create_override_for`]
    /// with `status` set to `cancelled`.
    pub async fn cancel_occurrence(&self, occurrence: &CalendarOccurrence) -> Result<OwnedEventId> {
        permissions::ensure_can_write(&self.room, "cancel occurrences of calendar events").await?;
        self.create_override_for(occurrence, |event| event.status = Status::Cancelled)
            .await
    }

    /// Builds a [`LiveCalendar`]: loads the room history (bounded by `options`, see
    /// [`Calendar::load_with`]) and keeps the index current with the calendar events received
    /// through sync.
    ///
    /// The sync event handler is registered before the history is paginated, and events it
    /// receives while loading are buffered and applied afterwards, so nothing that arrives in
    /// between is lost (the index ignores events it has already seen). The client must be
    /// syncing (for example with [`Client::sync`]) for live updates to arrive.
    pub async fn live(&self, options: LoadOptions) -> Result<LiveCalendar> {
        let shared = Arc::new(LiveShared {
            index: RwLock::new(CalendarIndex::new()),
            pending: Mutex::new(Some(Vec::new())),
            changes: broadcast::channel(CHANGE_CHANNEL_CAPACITY).0,
        });

        let handler_shared = Arc::downgrade(&shared);
        let handle = self
            .room
            .add_event_handler(move |raw: Raw<AnySyncTimelineEvent>| {
                let shared = handler_shared.clone();
                async move {
                    if let Some(shared) = shared.upgrade() {
                        shared.on_sync_event(raw).await;
                    }
                }
            });
        let guard = self.room.client().event_handler_drop_guard(handle);

        let mut loaded = CalendarIndex::new();
        self.load_into(&mut loaded, options).await?;

        {
            // Take the write lock before closing the buffer: a handler that finds the buffer
            // closed then waits for this lock and applies its event after the buffered ones.
            let mut index = shared.index.write().await;
            *index = loaded;
            let buffered = shared
                .pending
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take()
                .unwrap_or_default();
            for raw in &buffered {
                shared.apply(&mut index, raw);
            }
        }

        Ok(LiveCalendar {
            calendar: self.clone(),
            shared,
            _handler: guard,
        })
    }

    // -----------------------------------------------------------------------------------------
    // Permissions
    // -----------------------------------------------------------------------------------------

    /// Shares the calendar with `user_id` in the given `role`.
    ///
    /// The user's power level is set **first** (power levels can be set for users who are not
    /// in the room yet), then the user is invited, so the role is in place before they can
    /// join. If the user is already invited or joined, only the role is adjusted. The role
    /// is pinned explicitly, even if it equals the default role.
    ///
    /// Requires the owner role; members who may invite but not change roles (editors with
    /// [`CalendarPermissions::editors_can_invite`]) can invite users with the default role.
    /// Fails with [`Error::PermissionDenied`] otherwise, before anything is sent.
    ///
    /// The current power levels are read from the homeserver (not from the local store), so
    /// several calls in a row do not overwrite each other.
    pub async fn invite(&self, user_id: &UserId, role: CalendarRole) -> Result<()> {
        permissions::invite(&self.room, user_id, role).await
    }

    /// Shares the calendar read-only: [`Calendar::invite`] with [`CalendarRole::Reader`].
    pub async fn invite_reader(&self, user_id: &UserId) -> Result<()> {
        self.invite(user_id, CalendarRole::Reader).await
    }

    /// Shares the calendar read-write: [`Calendar::invite`] with [`CalendarRole::Editor`].
    pub async fn invite_editor(&self, user_id: &UserId) -> Result<()> {
        self.invite(user_id, CalendarRole::Editor).await
    }

    /// Changes the role of `user_id` (who need not be a member yet) by setting their power
    /// level to the role's level. Requires the owner role.
    ///
    /// Refuses ([`Error::PermissionDenied`]) to demote the current user if they are the last
    /// joined owner, to change the role of a user whose power level is not lower than the
    /// current user's, and to change room creators in room version 12 and later (they have
    /// unlimited power and are always owners; asking for `Owner` is a no-op). Fails with
    /// [`Error::InvalidPermissions`] if the room's power levels do not follow the SDK's
    /// scheme so that the level would not give the requested role (for example a reader level
    /// that can still send calendar events); call [`Calendar::apply_permissions`] first.
    pub async fn set_role(&self, user_id: &UserId, role: CalendarRole) -> Result<()> {
        permissions::set_role(&self.room, user_id, role).await
    }

    /// The role of `user_id`, derived from what the room's power levels allow them to do (see
    /// [`role_from_power_levels`](crate::permissions::role_from_power_levels)), so it is
    /// correct for calendars created by other clients too.
    ///
    /// Uses the locally synced power levels, or asks the homeserver if they have not been
    /// synced yet. Role changes, including those made with [`Calendar::set_role`] by this
    /// client, show up once they have come back through sync.
    pub async fn role_of(&self, user_id: &UserId) -> Result<CalendarRole> {
        permissions::role_of(&self.room, user_id).await
    }

    /// The current user's role, see [`Calendar::role_of`].
    pub async fn my_role(&self) -> Result<CalendarRole> {
        self.role_of(self.room.own_user_id()).await
    }

    /// Whether the current user may send calendar events (is an editor or owner).
    pub async fn can_write(&self) -> Result<bool> {
        Ok(self.my_role().await?.can_write())
    }

    /// The joined and invited members with their roles, sorted by role (owners first), then
    /// user ID. Fetches the member list from the homeserver if it has not been loaded yet;
    /// roles come from the power levels as in [`Calendar::role_of`].
    pub async fn members_with_roles(&self) -> Result<Vec<CalendarMember>> {
        permissions::members_with_roles(&self.room).await
    }

    /// The calendar's permission settings as read from its power levels, including whether
    /// they follow the SDK's scheme exactly.
    pub async fn permissions(&self) -> Result<CalendarPermissionsSummary> {
        permissions::permissions(&self.room).await
    }

    /// Rewrites the calendar's power levels to the SDK's scheme for `permissions` (see
    /// [`power_levels_for`](crate::permissions::power_levels_for)).
    ///
    /// Existing per-user levels are kept untouched (use
    /// [`Calendar::apply_permissions_normalizing_users`] to map them to role levels), as are
    /// levels for event types the scheme does not mention and unknown keys. Requires the owner
    /// role, and refuses changes that would take it away from the current user. Use this to
    /// bring calendars created by other clients, or with Matrix's default power levels (where
    /// every member can write), under the scheme.
    pub async fn apply_permissions(&self, permissions: &CalendarPermissions) -> Result<()> {
        permissions::apply_permissions(&self.room, permissions, false).await
    }

    /// Like [`Calendar::apply_permissions`], and also maps every explicit user level to the
    /// nearest role level (0, 50 or 100; ties go to the less privileged role). Users whose
    /// level is not lower than the current user's are left alone.
    pub async fn apply_permissions_normalizing_users(
        &self,
        permissions: &CalendarPermissions,
    ) -> Result<()> {
        permissions::apply_permissions(&self.room, permissions, true).await
    }

    /// Changes the role of members without an explicit role ([`CalendarRole::Reader`] or
    /// [`CalendarRole::Editor`]). This applies the scheme like [`Calendar::apply_permissions`],
    /// keeping the other current settings.
    pub async fn set_default_role(&self, role: CalendarRole) -> Result<()> {
        permissions::set_default_role(&self.room, role).await
    }

    /// Removes `user_id` from the calendar (a kick; works for invited and joined members) and
    /// clears their explicit role. Requires the owner role and a higher power level than the
    /// member's. In unencrypted calendars, the removed user can no longer read new events;
    /// in encrypted ones, new room keys are not shared with them anymore.
    pub async fn remove_member(&self, user_id: &UserId, reason: Option<&str>) -> Result<()> {
        permissions::remove_member(&self.room, user_id, reason).await
    }

    /// Stops sharing this calendar: leaves the calendar room and forgets it, so the calendar
    /// disappears from [`list_calendars`] and from the local store.
    ///
    /// This is the counterpart of [`CalendarInvitation::accept`] and the same thing as
    /// [`CalendarInvitation::decline`] after the fact. Forgetting means the room is also gone
    /// from `Client::left_rooms` and its history is dropped locally; an owner can invite the
    /// user again, and the calendar then arrives as a new [`CalendarInvitation`].
    ///
    /// **Leaving does not delete the calendar.** Matrix has no "delete room": the room keeps
    /// existing for the other members, and a calendar whose last member has left simply
    /// becomes unreachable. The SDK therefore does *not* refuse to let the last owner leave
    /// (unlike [`Calendar::set_role`], which refuses to demote the last owner, because that
    /// would leave a calendar nobody can manage while people are still in it). Make another
    /// member an owner with [`Calendar::set_role`] before leaving if the calendar should stay
    /// manageable.
    ///
    /// Leaving is still possible if forgetting fails (the homeserver may already have
    /// forgotten the room itself); the failure is only logged.
    pub async fn leave(&self) -> Result<()> {
        self.room.leave().await?;
        // `Room::leave` only forgets rooms that were invites, so forget joined ones here.
        if let Err(error) = self.room.forget().await {
            debug!(room_id = %self.room_id(), %error, "left the calendar but could not forget it");
        }
        Ok(())
    }

    async fn send(&self, content: CalendarEventEventContent) -> Result<OwnedEventId> {
        Ok(self.room.send(content).await?.response.event_id)
    }

    /// Fetches `event_id` and checks that it is a calendar event sent by the current user that
    /// is not itself an edit.
    async fn fetch_own_original(&self, event_id: &EventId) -> Result<ParsedCalendarEvent> {
        // Boxed: the future is deep enough to overflow the compiler's layout query limit.
        let event = Box::pin(self.room.load_or_fetch_event(event_id, None)).await?;
        let raw = event.raw().json();
        let envelope = parse_envelope(raw)?;
        if !ids::is_calendar_event_type(&envelope.event_type) {
            return Err(Error::Other(format!(
                "{event_id} is not a calendar event (type {})",
                envelope.event_type
            )));
        }
        let own_user_id = self.room.own_user_id();
        if envelope.sender.as_deref() != Some(own_user_id) {
            return Err(Error::PermissionDenied(format!(
                "{event_id} was sent by {}, not by {own_user_id}; only the original sender may \
                 modify a calendar event",
                envelope
                    .sender
                    .as_ref()
                    .map_or("an unknown sender", |s| s.as_str()),
            )));
        }
        if envelope.is_redacted() {
            return Err(Error::Other(format!("{event_id} has been redacted")));
        }
        let content = envelope
            .content
            .as_ref()
            .ok_or_else(|| Error::Other(format!("{event_id} has no content")))?;
        let parsed = parse_calendar_content(content)?;
        if matches!(parsed.relation, ParsedRelation::Replace(_)) {
            return Err(Error::Other(format!(
                "{event_id} is an edit; pass the ID of the original event"
            )));
        }
        Ok(parsed)
    }
}

/// Returns the joined rooms whose room type is a calendar room type (stable or unstable).
///
/// Calendars the current user has been invited to but has not accepted yet are *not*
/// included; see [`list_calendar_invitations`].
pub fn list_calendars(client: &Client) -> Vec<Calendar> {
    client
        .joined_rooms()
        .into_iter()
        .filter_map(|room| Calendar::try_from_room(room).ok())
        .collect()
}

fn is_calendar_room(room: &Room) -> bool {
    room.room_type()
        .is_some_and(|room_type| ids::is_calendar_room_type(room_type.as_str()))
}

/// Reads the `calendar_info` state event of `room` from the local state store, accepting both
/// the unstable and the stable event type (the unstable one wins if both are present) and both
/// synced and stripped state.
async fn read_calendar_info(room: &Room) -> Result<Option<CalendarInfoEventContent>> {
    for event_type in [ids::CALENDAR_INFO_UNSTABLE, ids::CALENDAR_INFO] {
        let Some(raw) = room
            .get_state_event(StateEventType::from(event_type), "")
            .await?
        else {
            continue;
        };
        let json = match &raw {
            RawAnySyncOrStrippedState::Sync(raw) => raw.json(),
            RawAnySyncOrStrippedState::Stripped(raw) => raw.json(),
        };
        match parse_state_content::<CalendarInfoEventContent>(json) {
            Ok(content) => return Ok(Some(content)),
            Err(error) => {
                warn!(%event_type, %error, "ignoring unparsable calendar_info state event");
            }
        }
    }
    Ok(None)
}

// ---------------------------------------------------------------------------------------------
// Sharing invitations (room membership)
// ---------------------------------------------------------------------------------------------

/// A pending invitation to a calendar room: another user shared their calendar with the
/// current user, who has neither accepted ([`accept`](Self::accept)) nor declined
/// ([`decline`](Self::decline)) yet.
///
/// This is *room membership*, i.e. sharing a whole calendar — not an invitation to a single
/// calendar event, which is [`CalendarInvite`](crate::events::CalendarInvite) in
/// [`scheduling`](crate::scheduling).
///
/// # What is known before accepting
///
/// A non-member can only see the **stripped state** the homeserver chose to put into the
/// invite, and may not read the room state from the homeserver. The stripped state always
/// identifies the room type (so invitations can be told apart from other room invites) and in
/// practice has `m.room.name`, `m.room.encryption` and the `m.room.member` events, but
/// homeservers do not have to include the `calendar_info` or `m.room.power_levels` state
/// events — and Synapse does not. [`color`](Self::color) is therefore usually `None` and
/// [`role`](Self::role) usually falls back; see their docs.
#[derive(Clone, Debug)]
pub struct CalendarInvitation {
    room: Room,
}

impl CalendarInvitation {
    /// Wraps `room` if it is a calendar room the current user has been invited to.
    ///
    /// Returns [`Error::NotACalendarRoom`] if the room type in the (stripped) create event is
    /// neither the stable nor the unstable calendar room type, or if the create event is
    /// missing from the stripped state, and [`Error::Other`] if the room is not an invite (for
    /// example because it has already been joined).
    pub fn try_from_room(room: Room) -> Result<CalendarInvitation> {
        if !is_calendar_room(&room) {
            return Err(Error::NotACalendarRoom(room.room_id().to_owned()));
        }
        if room.state() != RoomState::Invited {
            return Err(Error::Other(format!(
                "{} is not a pending calendar invitation: its membership is {:?}, not invited",
                room.room_id(),
                room.state(),
            )));
        }
        Ok(Self { room })
    }

    /// The underlying room, whose [state](Room::state) is [`RoomState::Invited`].
    pub fn room(&self) -> &Room {
        &self.room
    }

    /// The room ID of the calendar.
    pub fn room_id(&self) -> &RoomId {
        self.room.room_id()
    }

    /// The calendar's name: the stripped `m.room.name`, falling back to the name of the
    /// `calendar_info` state event if the stripped state contains it (see the
    /// [type docs](Self#what-is-known-before-accepting)).
    pub async fn name(&self) -> Option<String> {
        if let Some(name) = self.room.name().filter(|name| !name.is_empty()) {
            return Some(name);
        }
        self.info_or_none()
            .await
            .as_ref()
            .and_then(|info| info.name())
            .filter(|name| !name.is_empty())
            .map(ToOwned::to_owned)
    }

    /// The calendar's suggested display colour as `#rrggbb`, from the `calendar_info` state
    /// event, parsed exactly like [`Calendar::info`] does after joining (invalid colours are
    /// dropped).
    ///
    /// Almost always `None`: homeservers are not required to put the `calendar_info` state
    /// event into the stripped invite state, and Synapse does not (its
    /// `room_prejoin_state_types` only covers a fixed set of `m.room.*` events). Read the
    /// colour with [`Calendar::info`] after [`accept`](Self::accept)ing, or show a placeholder
    /// colour until then.
    pub async fn color(&self) -> Option<String> {
        self.info_or_none()
            .await
            .as_ref()
            .and_then(|info| info.info.valid_color())
            .map(ToOwned::to_owned)
    }

    /// The `calendar_info` state event, if the homeserver included it in the stripped invite
    /// state; see [`color`](Self::color) for why it is usually `None`. Reads the local store
    /// only, like [`Calendar::info`].
    pub async fn info(&self) -> Result<Option<CalendarInfoEventContent>> {
        read_calendar_info(&self.room).await
    }

    /// Who shared the calendar: the sender of the current user's `m.room.member` invite event
    /// in the stripped state.
    ///
    /// `None` only if the invite's member event is missing, which should not happen.
    pub async fn inviter(&self) -> Option<OwnedUserId> {
        match self.room.invite_details().await {
            Ok(invite) => Some(invite.inviter_id),
            Err(error) => {
                debug!(room_id = %self.room_id(), %error, "no invite details for the invitation");
                None
            }
        }
    }

    /// The role the current user would have after accepting, derived from the stripped
    /// `m.room.power_levels` with
    /// [`role_from_power_levels`](crate::permissions::role_from_power_levels).
    ///
    /// **Informational and a lower bound.** Homeservers are not required to include
    /// `m.room.power_levels` in the stripped invite state, and Synapse does not; an invited
    /// user cannot read the room state from the homeserver either. When the power levels are
    /// unknown this returns [`CalendarRole::Reader`], the SDK's default role for members
    /// without an explicit role (least privilege, see
    /// [`CalendarPermissions::default`](crate::permissions::CalendarPermissions::default)) —
    /// so an invitation as an editor also shows up as `Reader` before accepting. The role the
    /// homeserver actually enforces is the one [`Calendar::my_role`] reports after
    /// [`accept`](Self::accept)ing.
    pub async fn role(&self) -> CalendarRole {
        permissions::local_role(&self.room, self.room.own_user_id())
            .await
            .unwrap_or(CalendarRole::Reader)
    }

    /// Whether the calendar is end-to-end encrypted, from the `m.room.encryption` state event
    /// in the stripped invite state (which Synapse does include).
    ///
    /// This reads the state event itself rather than [`Room::encryption_state`], which stays
    /// [`Unknown`](matrix_sdk::room::EncryptionState::Unknown) until the room is joined and
    /// synced. `false` therefore means "no usable `m.room.encryption` in the stripped state";
    /// on a homeserver that withholds it, the calendar may still turn out to be encrypted
    /// after [`accept`](Self::accept)ing.
    pub async fn is_encrypted(&self) -> bool {
        let Ok(Some(raw)) = self
            .room
            .get_state_event(StateEventType::RoomEncryption, "")
            .await
        else {
            return false;
        };
        let json = match &raw {
            RawAnySyncOrStrippedState::Sync(raw) => raw.json(),
            RawAnySyncOrStrippedState::Stripped(raw) => raw.json(),
        };
        // A redacted (or otherwise algorithm-less) event does not make the room encrypted,
        // the same rule `matrix-sdk` applies to synced state.
        parse_state_content::<RoomEncryptionEventContent>(json).is_ok()
    }

    /// Accepts the invitation: joins the calendar room and returns the [`Calendar`].
    ///
    /// Like after [`Calendar::create`], the room's full state has not been synced yet when
    /// this returns: only the stripped invite state is in the local store. The returned
    /// `Calendar` is usable right away — [`Calendar::my_role`],
    /// [`Calendar::members_with_roles`] and [`Calendar::permissions`] fall back to the
    /// homeserver, and [`Calendar::load`] paginates over `/messages` — but
    /// [`Calendar::info`], which only reads the local store, and
    /// [`list_calendars`](crate::calendar::list_calendars) see the calendar once the join has
    /// come back through `/sync`.
    pub async fn accept(self) -> Result<Calendar> {
        self.room.join().await?;
        // The room type is known from the stripped create event, so this is a calendar even
        // though the joined room state has not been synced yet.
        Ok(Calendar { room: self.room })
    }

    /// Declines the invitation: leaves the calendar room.
    ///
    /// `matrix-sdk` also forgets rooms that were invites when leaving them, so the room is
    /// dropped from the local store and does not linger in `Client::left_rooms`; no separate
    /// `forget` call is needed (unlike [`Calendar::leave`], which leaves a *joined* room).
    /// Declining is not visible to the other members beyond the leave event, and an owner can
    /// invite the user again.
    pub async fn decline(self) -> Result<()> {
        self.room.leave().await?;
        Ok(())
    }

    /// [`info`](Self::info) with store errors logged and swallowed, for the infallible
    /// accessors.
    async fn info_or_none(&self) -> Option<CalendarInfoEventContent> {
        match self.info().await {
            Ok(info) => info,
            Err(error) => {
                debug!(room_id = %self.room_id(), %error, "could not read the calendar info");
                None
            }
        }
    }
}

/// Returns the pending invitations to calendar rooms: the rooms the current user has been
/// invited to whose room type (in the stripped create event the homeserver includes in the
/// invite) is a calendar room type, stable or unstable.
///
/// Invitations to non-calendar rooms are skipped, so this is safe to call on a client that is
/// also used for chat. New invitations appear here once they have come in through `/sync`;
/// register a `m.room.member` event handler, or watch
/// [`Client::sync_stream`](matrix_sdk::Client::sync_stream), to be notified.
pub fn list_calendar_invitations(client: &Client) -> Vec<CalendarInvitation> {
    client
        .invited_rooms()
        .into_iter()
        .filter_map(|room| CalendarInvitation::try_from_room(room).ok())
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Raw event parsing
// ---------------------------------------------------------------------------------------------

/// The envelope fields of a raw timeline event, in either sync or room-id-bearing format.
#[derive(Debug, Deserialize)]
struct RawEnvelope {
    #[serde(rename = "type")]
    event_type: String,
    event_id: Option<OwnedEventId>,
    sender: Option<OwnedUserId>,
    origin_server_ts: Option<MilliSecondsSinceUnixEpoch>,
    content: Option<serde_json::Value>,
    /// Top-level `redacts` of `m.room.redaction` in room versions before 11.
    redacts: Option<OwnedEventId>,
    unsigned: Option<RawUnsigned>,
}

#[derive(Debug, Deserialize)]
struct RawUnsigned {
    redacted_because: Option<serde::de::IgnoredAny>,
}

impl RawEnvelope {
    fn is_redacted(&self) -> bool {
        self.unsigned
            .as_ref()
            .is_some_and(|u| u.redacted_because.is_some())
    }

    /// The event redacted by an `m.room.redaction` event (`content.redacts` in room version
    /// 11+, top-level `redacts` before).
    fn redacts(&self) -> Option<OwnedEventId> {
        self.content
            .as_ref()
            .and_then(|c| c.get("redacts"))
            .and_then(|v| OwnedEventId::deserialize(v).ok())
            .or_else(|| self.redacts.clone())
    }
}

fn parse_envelope(json: &RawJsonValue) -> Result<RawEnvelope> {
    Ok(serde_json::from_str(json.get())?)
}

fn parse_state_content<C: for<'de> Deserialize<'de>>(json: &RawJsonValue) -> Result<C> {
    #[derive(Deserialize)]
    struct StateEnvelope {
        content: serde_json::Value,
    }
    let envelope: StateEnvelope = serde_json::from_str(json.get())?;
    Ok(C::deserialize(&envelope.content)?)
}

/// The relation of a calendar event, as relevant for the index.
#[derive(Debug)]
enum ParsedRelation {
    /// No relation (or a relation the index does not interpret, such as a thread).
    None,
    /// An `m.replace` edit of the given event.
    Replace(OwnedEventId),
    /// An instance override of the given series event.
    Override(OwnedEventId),
}

#[derive(Debug)]
struct ParsedCalendarEvent {
    relation: ParsedRelation,
    /// For edits the `m.new_content` block, otherwise the top-level block.
    event: CalendarEvent,
}

/// Parses the content of a calendar event, accepting both the stable and the unstable block
/// and `rel_type` names.
///
/// Edits are read from `m.new_content` only, so the top-level fallback block may be absent.
fn parse_calendar_content(
    content: &serde_json::Value,
) -> std::result::Result<ParsedCalendarEvent, serde_json::Error> {
    #[derive(Deserialize)]
    struct RelatesTo {
        rel_type: Option<String>,
        event_id: Option<OwnedEventId>,
    }

    let relates_to = content
        .get("m.relates_to")
        .map(RelatesTo::deserialize)
        .transpose()?;
    let (relation, block) = match relates_to {
        Some(RelatesTo {
            rel_type: Some(rel_type),
            event_id: Some(event_id),
        }) if rel_type == "m.replace" => {
            let new_content = content
                .get("m.new_content")
                .ok_or_else(|| serde_json::Error::missing_field("m.new_content"))?;
            (ParsedRelation::Replace(event_id), new_content)
        }
        Some(RelatesTo {
            rel_type: Some(rel_type),
            event_id: Some(event_id),
        }) if ids::is_calendar_override_rel_type(&rel_type) => {
            (ParsedRelation::Override(event_id), content)
        }
        _ => (ParsedRelation::None, content),
    };
    let event = CalendarEventEventContentWithoutRelation::deserialize(block)?.event;
    Ok(ParsedCalendarEvent { relation, event })
}

// ---------------------------------------------------------------------------------------------
// Index
// ---------------------------------------------------------------------------------------------

pub use crate::key::EventKey;

/// A calendar event in a [`CalendarIndex`], with edits and overrides applied.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct IndexedEvent {
    /// The `(sender, uid)` key.
    pub key: EventKey,
    /// The ID of the original event; stable across edits. Use it for
    /// [`Calendar::update_event`], [`Calendar::cancel_event`] and
    /// [`Calendar::create_override`].
    pub event_id: OwnedEventId,
    /// The sender of the original event.
    pub sender: OwnedUserId,
    /// The `origin_server_ts` of the original event.
    pub origin_server_ts: MilliSecondsSinceUnixEpoch,
    /// The ID of the edit whose content is in effect, if any.
    pub last_edit_event_id: Option<OwnedEventId>,
    /// The effective content: the newest valid edit's `m.new_content`, or the original.
    pub content: CalendarEvent,
    /// Instance overrides of this (recurring) event, one per `recurrence_id`, sorted by
    /// `recurrence_id`.
    pub overrides: Vec<IndexedOverride>,
}

impl IndexedEvent {
    /// The effective organiser: the content's `organizer`, or the sender of the original
    /// event if the content names none.
    pub fn organizer(&self) -> &UserId {
        self.content.organizer.as_deref().unwrap_or(&self.sender)
    }
}

/// An instance override attached to an [`IndexedEvent`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct IndexedOverride {
    /// The ID of the override event; stable across edits of the override.
    pub event_id: OwnedEventId,
    /// The `origin_server_ts` of the override event.
    pub origin_server_ts: MilliSecondsSinceUnixEpoch,
    /// The ID of the edit of the override whose content is in effect, if any.
    pub last_edit_event_id: Option<OwnedEventId>,
    /// The overridden occurrence's original UTC start (the effective content's
    /// `recurrence_id`).
    pub recurrence_id: DateTime<Utc>,
    /// The effective override content.
    pub content: CalendarEvent,
}

/// A concrete occurrence of a calendar event, together with where it came from in the room.
///
/// Returned by [`CalendarIndex::occurrences`] and [`CalendarIndex::next_occurrence`]; pass it
/// to [`Calendar::create_override_for`] or [`Calendar::cancel_occurrence`] to change this one
/// instance.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CalendarOccurrence {
    /// The `(sender, uid)` key of the event or series.
    pub key: EventKey,
    /// The ID of the original event of the event or series (see [`IndexedEvent::event_id`]).
    pub event_id: OwnedEventId,
    /// The ID of the override event that replaces this instance, if any (see
    /// [`IndexedOverride::event_id`]).
    pub override_event_id: Option<OwnedEventId>,
    /// The occurrence itself. Use its `start`/`end` for the instance's times: for instances
    /// that are not overridden, [`Occurrence::event`] is the unchanged series master.
    pub occurrence: Occurrence,
}

impl CalendarOccurrence {
    fn new(entry: &IndexedEvent, occurrence: Occurrence) -> Self {
        let override_event_id = if occurrence.is_override {
            entry
                .overrides
                .iter()
                .find(|o| Some(o.recurrence_id) == occurrence.recurrence_id)
                .map(|o| o.event_id.clone())
        } else {
            None
        };
        Self {
            key: entry.key.clone(),
            event_id: entry.event_id.clone(),
            override_event_id,
            occurrence,
        }
    }

    /// The UTC start of this occurrence; all-day dates resolve to midnight in `fallback_tz`.
    pub fn start_utc(&self, fallback_tz: Tz) -> DateTime<Utc> {
        self.occurrence.start_utc(fallback_tz)
    }

    /// The UTC end of this occurrence; all-day dates resolve to midnight in `fallback_tz`.
    pub fn end_utc(&self, fallback_tz: Tz) -> DateTime<Utc> {
        self.occurrence.end_utc(fallback_tz)
    }

    /// The effective event of this instance with `start`/`end` set to the occurrence's times
    /// (and without `recurrence`), i.e. the instance as a standalone event.
    pub fn into_instance(self) -> CalendarEvent {
        let mut event = self.occurrence.event;
        event.start = self.occurrence.start;
        event.end = self.occurrence.end;
        event.recurrence = None;
        event
    }

    fn sort_key(
        &self,
        fallback_tz: Tz,
    ) -> (
        DateTime<Utc>,
        DateTime<Utc>,
        EventKey,
        Option<DateTime<Utc>>,
    ) {
        (
            self.start_utc(fallback_tz),
            self.end_utc(fallback_tz),
            self.key.clone(),
            self.occurrence.recurrence_id,
        )
    }
}

/// The effective contents of the overrides of `entry`.
fn override_contents(entry: &IndexedEvent) -> impl Iterator<Item = &CalendarEvent> {
    // Expansion normalizes all-day recurrence IDs using the query's fallback timezone.
    // Different encodings may then identify the same day; expansion keeps the last
    // override, so feed it revisions in precedence order rather than raw-ID order.
    let mut overrides: Vec<_> = entry.overrides.iter().collect();
    overrides
        .sort_by(|a, b| (a.origin_server_ts, &a.event_id).cmp(&(b.origin_server_ts, &b.event_id)));
    overrides.into_iter().map(|o| &o.content)
}

/// A calendar event that is not an edit: a plain original or an override.
#[derive(Clone, Debug)]
struct Node {
    event_id: OwnedEventId,
    sender: OwnedUserId,
    ts: MilliSecondsSinceUnixEpoch,
    content: CalendarEvent,
    /// For overrides: the series' original event.
    series: Option<OwnedEventId>,
}

impl Node {
    fn key(&self) -> EventKey {
        EventKey::new(self.sender.clone(), self.content.uid.clone())
    }
}

#[derive(Clone, Debug)]
struct Edit {
    event_id: OwnedEventId,
    sender: OwnedUserId,
    ts: MilliSecondsSinceUnixEpoch,
    content: CalendarEvent,
}

/// An in-memory view of the calendar events in a room, built from timeline events.
///
/// Feed it events in any order (for example backwards pagination, then live sync) with
/// [`CalendarIndex::apply_raw_sync`], [`CalendarIndex::apply_raw`] or
/// [`CalendarIndex::apply_timeline_event`]. The rules follow MSC4496:
///
/// - Only `calendar_event` events (stable or unstable type) are indexed, plus
///   `m.room.redaction` and redacted events, which remove what they redact.
/// - A plain event with uid `U` from sender `S` creates the entry `(S, U)`. If several
///   originals share a key, the one with the earliest `origin_server_ts` is canonical and the
///   others are ignored.
/// - An `m.replace` edit applies only if sent by the original's sender and keeping the `uid`;
///   the newest valid edit (by `origin_server_ts`) wins. Edits that arrive before their
///   original are buffered.
/// - An override (`calendar_override` relation) attaches to the canonical original it points
///   at if it has the same sender and `uid` and a `recurrence_id`. Overrides are keyed by
///   `recurrence_id` (the newest override event wins) and can themselves be edited.
/// - Redacting an original removes the entry, redacting an edit reverts to the previous
///   effective content, and redacting an override removes the override.
/// - The `organizer` field is optional; [`IndexedEvent::organizer`] falls back to the sender.
/// - Events whose content fails to parse are skipped with a warning.
#[derive(Clone, Debug, Default)]
pub struct CalendarIndex {
    /// Plain originals and overrides by event ID.
    nodes: HashMap<OwnedEventId, Node>,
    /// Node IDs per key (overrides are keyed by their own sender and uid).
    nodes_by_key: HashMap<EventKey, HashSet<OwnedEventId>>,
    /// Edits per target event ID, sorted by `(ts, event_id)`.
    edits: HashMap<OwnedEventId, Vec<Edit>>,
    /// Edit ID to target ID.
    edit_targets: HashMap<OwnedEventId, OwnedEventId>,
    /// IDs of redacted events, so that they are ignored if they arrive after the redaction
    /// (backward pagination delivers a redaction before the event it redacts). Redactions of
    /// unrelated events cannot be told apart from those of calendar events not seen yet, so
    /// every redaction target is remembered, up to [`MAX_REMEMBERED_REDACTIONS`] entries.
    redacted: HashSet<OwnedEventId>,
    /// The derived entries.
    entries: HashMap<EventKey, IndexedEvent>,
    /// Original and override event IDs to entry keys.
    by_id: HashMap<OwnedEventId, EventKey>,
}

impl CalendarIndex {
    /// Creates an empty index.
    pub fn new() -> Self {
        Self::default()
    }

    /// Applies a sync-format timeline event (as received from `/sync`, or from
    /// [`TimelineEvent::raw`]).
    ///
    /// Returns whether the visible content of the index changed. Events of unrelated types
    /// are ignored. Calendar events with unparsable content are skipped with a warning; an
    /// error is only returned if the event envelope itself is malformed (for example a
    /// calendar event without `event_id` or `sender`).
    pub fn apply_raw_sync(&mut self, raw: &Raw<AnySyncTimelineEvent>) -> Result<bool> {
        Ok(self.apply_json(raw.json())?.is_some())
    }

    /// Applies a timeline event that carries a `room_id` (as returned by `/messages` or
    /// `/event`). See [`CalendarIndex::apply_raw_sync`].
    pub fn apply_raw(&mut self, raw: &Raw<AnyTimelineEvent>) -> Result<bool> {
        Ok(self.apply_json(raw.json())?.is_some())
    }

    /// Applies a [`TimelineEvent`] as returned by matrix-sdk (for example from
    /// [`Room::messages`]), using the decrypted event if it was decrypted. Events that could
    /// not be decrypted are ignored. See [`CalendarIndex::apply_raw_sync`].
    pub fn apply_timeline_event(&mut self, event: &TimelineEvent) -> Result<bool> {
        Ok(self.apply_json(event.raw().json())?.is_some())
    }

    /// The entry for `key`.
    pub fn get(&self, key: &EventKey) -> Option<&IndexedEvent> {
        self.entries.get(key)
    }

    /// The entry whose original event, or one of whose attached overrides, has ID `id`.
    pub fn by_event_id(&self, id: &EventId) -> Option<&IndexedEvent> {
        self.by_id.get(id).and_then(|key| self.entries.get(key))
    }

    /// Iterates over all entries, in no particular order.
    pub fn iter(&self) -> impl Iterator<Item = &IndexedEvent> {
        self.entries.values()
    }

    /// The number of entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the index has no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// A coarse selection of the entries that may have occurrences in `[start, end)`, sorted
    /// by start time.
    ///
    /// Non-recurring events are included if their `start`/`end` overlap the range; all-day
    /// dates are interpreted in UTC with one day of slack on either side, since the viewer's
    /// timezone is unknown here. Recurring events (with a `recurrence`) are **always**
    /// included: expanding them into occurrences, applying overrides and filtering precisely
    /// is the job of the [`recurrence`] module. Cancelled events are
    /// included as well; callers decide whether to show them.
    pub fn events_in_range(&self, start: DateTime<Utc>, end: DateTime<Utc>) -> Vec<&IndexedEvent> {
        let mut result: Vec<&IndexedEvent> = self
            .entries
            .values()
            .filter(|entry| {
                let content = &entry.content;
                if content.is_recurring() {
                    return true;
                }
                let slack = if content.all_day || content.start.is_all_day() {
                    TimeDelta::days(1)
                } else {
                    TimeDelta::zero()
                };
                // Saturating: dates near the ends of chrono's range must not panic.
                let ev_start = content
                    .start
                    .to_utc(chrono_tz::UTC)
                    .checked_sub_signed(slack)
                    .unwrap_or(DateTime::<Utc>::MIN_UTC);
                let ev_end = content
                    .end
                    .to_utc(chrono_tz::UTC)
                    .checked_add_signed(slack)
                    .unwrap_or(DateTime::<Utc>::MAX_UTC);
                // Zero-length events at `start` still count.
                ev_start < end && (ev_end > start || ev_start == start)
            })
            .collect();
        result.sort_by(|a, b| {
            let a_start = a.content.start.to_utc(chrono_tz::UTC);
            let b_start = b.content.start.to_utc(chrono_tz::UTC);
            a_start
                .cmp(&b_start)
                .then_with(|| a.event_id.cmp(&b.event_id))
        });
        result
    }

    /// Expands every event into its concrete occurrences within `[start, end)`, sorted by UTC
    /// start (then end, key and original start).
    ///
    /// Recurring series are expanded with their overrides (see [`recurrence::expand`]);
    /// non-recurring events yield one occurrence if they intersect the range. Cancelled events
    /// and series, and cancelled instances, are excluded. All-day dates are placed on the
    /// timeline at midnight in `fallback_tz`.
    ///
    /// A series that cannot be expanded (for example because of an invalid recurrence rule)
    /// is skipped with a warning; it does not fail the query. Every series yields at most
    /// [`recurrence::MAX_SCANNED_INSTANCES`] generated instances.
    pub fn occurrences(
        &self,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        fallback_tz: Tz,
    ) -> Result<Vec<CalendarOccurrence>> {
        let opts = ExpandOptions::new(start, end)
            .with_limit(None)
            .with_fallback_tz(fallback_tz);
        let mut out = Vec::new();
        for entry in self.active_entries() {
            match recurrence::expand(&entry.content, override_contents(entry), &opts) {
                Ok(occurrences) => out.extend(
                    occurrences
                        .into_iter()
                        .filter(|occ| !occ.event.is_cancelled())
                        .map(|occ| CalendarOccurrence::new(entry, occ)),
                ),
                Err(error) => warn!(
                    event_id = %entry.event_id,
                    %error,
                    "skipping calendar event that cannot be expanded"
                ),
            }
        }
        out.sort_by_cached_key(|o| o.sort_key(fallback_tz));
        Ok(out)
    }

    /// The first occurrence of any event that starts strictly after `after`, or `None`.
    ///
    /// Cancelled events, series and instances are skipped, as are series that cannot be
    /// expanded (with a warning). Occurrences already in progress at `after` are not returned.
    /// All-day dates are placed on the timeline at midnight in `fallback_tz`. Ties are broken
    /// as in [`CalendarIndex::occurrences`].
    pub fn next_occurrence(
        &self,
        after: DateTime<Utc>,
        fallback_tz: Tz,
    ) -> Result<Option<CalendarOccurrence>> {
        let mut best: Option<CalendarOccurrence> = None;
        for entry in self.active_entries() {
            let overrides = override_contents(entry);
            let occ =
                match recurrence::next_occurrence(&entry.content, overrides, after, fallback_tz) {
                    Ok(Some(occ)) if !occ.event.is_cancelled() => occ,
                    Ok(_) => continue,
                    Err(error) => {
                        warn!(
                            event_id = %entry.event_id,
                            %error,
                            "skipping calendar event that cannot be expanded"
                        );
                        continue;
                    }
                };
            let candidate = CalendarOccurrence::new(entry, occ);
            if best
                .as_ref()
                .is_none_or(|b| candidate.sort_key(fallback_tz) < b.sort_key(fallback_tz))
            {
                best = Some(candidate);
            }
        }
        Ok(best)
    }

    /// The busy intervals within `[start, end)`: [`CalendarIndex::occurrences`] fed into
    /// [`availability::compute_busy_from_occurrences`].
    ///
    /// Transparent occurrences are not busy, tentative ones are
    /// [`BusyType::BusyTentative`](crate::events::BusyType::BusyTentative). With
    /// `include_titles` every interval carries its event's title and intervals are not merged.
    pub fn busy(
        &self,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        fallback_tz: Tz,
        include_titles: bool,
    ) -> Result<Vec<BusyInterval>> {
        let occurrences = self.occurrences(start, end, fallback_tz)?;
        Ok(availability::compute_busy_from_occurrences(
            occurrences.iter().map(|o| &o.occurrence),
            start,
            end,
            fallback_tz,
            include_titles,
        ))
    }

    /// The entries that are not cancelled.
    fn active_entries(&self) -> impl Iterator<Item = &IndexedEvent> {
        self.entries
            .values()
            .filter(|entry| !entry.content.is_cancelled())
    }

    /// Applies a raw event; returns the key of the entry whose visible content changed, if any
    /// (an event affects at most one entry).
    fn apply_json(&mut self, json: &RawJsonValue) -> Result<Option<EventKey>> {
        #[derive(Deserialize)]
        struct TypeOnly {
            #[serde(rename = "type")]
            event_type: String,
        }
        let TypeOnly { event_type } = serde_json::from_str(json.get())?;

        if event_type == REDACTION {
            let envelope = parse_envelope(json)?;
            return Ok(match envelope.redacts() {
                Some(redacted) => self.redact(redacted),
                None => {
                    warn!(event_id = ?envelope.event_id, "ignoring redaction without `redacts`");
                    None
                }
            });
        }
        if !ids::is_calendar_event_type(&event_type) {
            return Ok(None);
        }

        let envelope = parse_envelope(json)?;
        let missing = |field: &str| Error::Other(format!("calendar event without `{field}`"));
        let event_id = envelope
            .event_id
            .clone()
            .ok_or_else(|| missing("event_id"))?;

        if envelope.is_redacted() {
            return Ok(self.redact(event_id));
        }
        if self.redacted.contains(&event_id)
            || self.nodes.contains_key(&event_id)
            || self.edit_targets.contains_key(&event_id)
        {
            return Ok(None);
        }

        let sender = envelope.sender.clone().ok_or_else(|| missing("sender"))?;
        let ts = envelope
            .origin_server_ts
            .ok_or_else(|| missing("origin_server_ts"))?;
        let Some(content) = envelope.content.as_ref() else {
            warn!(%event_id, "skipping calendar event without content");
            return Ok(None);
        };
        let parsed = match parse_calendar_content(content) {
            Ok(parsed) => parsed,
            Err(error) => {
                warn!(%event_id, %error, "skipping calendar event with invalid content");
                return Ok(None);
            }
        };

        match parsed.relation {
            ParsedRelation::Replace(target) => Ok(self.add_edit(
                target,
                Edit {
                    event_id,
                    sender,
                    ts,
                    content: parsed.event,
                },
            )),
            ParsedRelation::Override(series) => Ok(self.add_node(Node {
                event_id,
                sender,
                ts,
                content: parsed.event,
                series: Some(series),
            })),
            ParsedRelation::None => Ok(self.add_node(Node {
                event_id,
                sender,
                ts,
                content: parsed.event,
                series: None,
            })),
        }
    }

    fn add_node(&mut self, node: Node) -> Option<EventKey> {
        let key = node.key();
        if let Some(edits) = self.edits.get(&node.event_id) {
            for edit in edits.iter().filter(|e| e.sender != node.sender) {
                warn!(
                    edit = %edit.event_id,
                    original = %node.event_id,
                    "ignoring edit by a different sender than the original"
                );
            }
        }
        self.nodes_by_key
            .entry(key.clone())
            .or_default()
            .insert(node.event_id.clone());
        self.nodes.insert(node.event_id.clone(), node);
        self.recompute(&key).then_some(key)
    }

    fn add_edit(&mut self, target: OwnedEventId, edit: Edit) -> Option<EventKey> {
        if let Some(node) = self.nodes.get(&target)
            && node.sender != edit.sender
        {
            warn!(
                edit = %edit.event_id,
                original = %target,
                "ignoring edit by a different sender than the original"
            );
        }
        self.edit_targets
            .insert(edit.event_id.clone(), target.clone());
        let edits = self.edits.entry(target.clone()).or_default();
        let pos = edits.partition_point(|e| (e.ts, &e.event_id) < (edit.ts, &edit.event_id));
        edits.insert(pos, edit);
        match self.nodes.get(&target) {
            Some(node) => {
                let key = node.key();
                self.recompute(&key).then_some(key)
            }
            // Buffered until the original arrives.
            None => None,
        }
    }

    /// Records that `event_id` was redacted and removes whatever it was.
    fn redact(&mut self, event_id: OwnedEventId) -> Option<EventKey> {
        if self.redacted.contains(&event_id) {
            return None;
        }
        if self.redacted.len() >= MAX_REMEMBERED_REDACTIONS {
            // Bound the memory use in busy rooms; forgetting an arbitrary old redaction only
            // matters if its target is delivered again later.
            if let Some(evicted) = self.redacted.iter().next().cloned() {
                self.redacted.remove(&evicted);
            }
        }
        self.redacted.insert(event_id.clone());
        if let Some(node) = self.nodes.remove(&event_id) {
            let key = node.key();
            if let Some(ids) = self.nodes_by_key.get_mut(&key) {
                ids.remove(&event_id);
                if ids.is_empty() {
                    self.nodes_by_key.remove(&key);
                }
            }
            return self.recompute(&key).then_some(key);
        }
        if let Some(target) = self.edit_targets.remove(&event_id) {
            if let Some(edits) = self.edits.get_mut(&target) {
                edits.retain(|e| e.event_id != event_id);
                if edits.is_empty() {
                    self.edits.remove(&target);
                }
            }
            if let Some(key) = self.nodes.get(&target).map(Node::key) {
                return self.recompute(&key).then_some(key);
            }
        }
        None
    }

    /// The effective content of `node` after applying its newest valid edit.
    fn effective(&self, node: &Node) -> (CalendarEvent, Option<OwnedEventId>) {
        let valid_edit = self.edits.get(&node.event_id).and_then(|edits| {
            edits
                .iter()
                .rev()
                .find(|e| e.sender == node.sender && e.content.uid == node.content.uid)
        });
        match valid_edit {
            Some(edit) => (edit.content.clone(), Some(edit.event_id.clone())),
            None => (node.content.clone(), None),
        }
    }

    /// Rebuilds the entry for `key`; returns whether it changed.
    fn recompute(&mut self, key: &EventKey) -> bool {
        let old = self.entries.remove(key);
        if let Some(old) = &old {
            self.by_id.remove(&old.event_id);
            for o in &old.overrides {
                self.by_id.remove(&o.event_id);
            }
        }

        let new = self.build_entry(key);
        if let Some(new) = &new {
            self.by_id.insert(new.event_id.clone(), key.clone());
            for o in &new.overrides {
                self.by_id.insert(o.event_id.clone(), key.clone());
            }
        }
        let changed = old != new;
        if let Some(new) = new {
            self.entries.insert(key.clone(), new);
        }
        changed
    }

    fn build_entry(&self, key: &EventKey) -> Option<IndexedEvent> {
        let nodes: Vec<&Node> = self
            .nodes_by_key
            .get(key)?
            .iter()
            .filter_map(|id| self.nodes.get(id))
            .collect();

        let mut originals: Vec<&Node> = nodes
            .iter()
            .copied()
            .filter(|n| n.series.is_none())
            .collect();
        originals.sort_by(|a, b| (a.ts, &a.event_id).cmp(&(b.ts, &b.event_id)));
        let (canonical, duplicates) = originals.split_first()?;
        for duplicate in duplicates {
            warn!(
                ignored = %duplicate.event_id,
                canonical = %canonical.event_id,
                sender = %key.sender,
                uid = %key.uid,
                "ignoring second original calendar event with the same sender and uid"
            );
        }

        let (content, last_edit_event_id) = self.effective(canonical);

        let mut overrides: BTreeMap<DateTime<Utc>, (&Node, IndexedOverride)> = BTreeMap::new();
        for node in nodes
            .iter()
            .copied()
            .filter(|n| n.series.as_ref() == Some(&canonical.event_id))
        {
            let (content, last_edit_event_id) = self.effective(node);
            let Some(recurrence_id) = content.recurrence_id else {
                warn!(event_id = %node.event_id, "ignoring override without recurrence_id");
                continue;
            };
            let candidate = IndexedOverride {
                event_id: node.event_id.clone(),
                origin_server_ts: node.ts,
                last_edit_event_id,
                recurrence_id,
                content,
            };
            match overrides.get(&recurrence_id) {
                Some((existing, _))
                    if (existing.ts, &existing.event_id) > (node.ts, &node.event_id) => {}
                _ => {
                    overrides.insert(recurrence_id, (node, candidate));
                }
            }
        }

        Some(IndexedEvent {
            key: key.clone(),
            event_id: canonical.event_id.clone(),
            sender: canonical.sender.clone(),
            origin_server_ts: canonical.ts,
            last_edit_event_id,
            content,
            overrides: overrides.into_values().map(|(_, o)| o).collect(),
        })
    }
}

// ---------------------------------------------------------------------------------------------
// Live index
// ---------------------------------------------------------------------------------------------

/// A change notification of a [`LiveCalendar`]: the visible content of the entry `key`
/// changed (it was added, edited, cancelled, removed, or one of its overrides changed).
///
/// Look the entry up with [`CalendarIndex::get`]; it is absent if it was removed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexChange {
    /// The key of the entry that changed.
    pub key: EventKey,
}

/// A shared, live-updating [`CalendarIndex`] of a calendar room, created with
/// [`Calendar::live`].
///
/// It loads the room history once and then keeps itself current from sync through an event
/// handler registered on the room; the handler is removed when the `LiveCalendar` is dropped.
/// Changes are announced on a broadcast channel ([`LiveCalendar::subscribe`]).
///
/// Only events delivered by sync are seen: if a sync response has a gap (a `limited`
/// timeline), events in the gap are not applied; create a new `LiveCalendar` to catch up.
/// Events that matrix-sdk cannot decrypt when they arrive are not applied either.
#[derive(Debug)]
pub struct LiveCalendar {
    calendar: Calendar,
    shared: Arc<LiveShared>,
    _handler: EventHandlerDropGuard,
}

impl LiveCalendar {
    /// The calendar this index belongs to.
    pub fn calendar(&self) -> &Calendar {
        &self.calendar
    }

    /// Read access to the current index.
    ///
    /// Sync processing waits while the guard is held, so do not hold it for long.
    pub async fn read(&self) -> RwLockReadGuard<'_, CalendarIndex> {
        self.shared.index.read().await
    }

    /// Subscribes to change notifications. An [`IndexChange`] is sent whenever an event
    /// received from sync changes the visible content of an entry.
    ///
    /// The channel is bounded; a receiver that falls behind gets
    /// [`RecvError::Lagged`](tokio::sync::broadcast::error::RecvError::Lagged) and should then
    /// re-read the whole index.
    pub fn subscribe(&self) -> broadcast::Receiver<IndexChange> {
        self.shared.changes.subscribe()
    }
}

/// State shared between a [`LiveCalendar`] and its sync event handler.
#[derive(Debug)]
struct LiveShared {
    index: RwLock<CalendarIndex>,
    /// Sync events received while the history is being loaded; `None` once loaded.
    pending: Mutex<Option<Vec<Raw<AnySyncTimelineEvent>>>>,
    changes: broadcast::Sender<IndexChange>,
}

impl LiveShared {
    async fn on_sync_event(&self, raw: Raw<AnySyncTimelineEvent>) {
        {
            let mut pending = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(buffer) = pending.as_mut() {
                buffer.push(raw);
                return;
            }
        }
        let mut index = self.index.write().await;
        self.apply(&mut index, &raw);
    }

    fn apply(&self, index: &mut CalendarIndex, raw: &Raw<AnySyncTimelineEvent>) {
        match index.apply_json(raw.json()) {
            Ok(Some(key)) => {
                // Sending only fails if there are no receivers, which is fine.
                let _ = self.changes.send(IndexChange { key });
            }
            Ok(None) => {}
            Err(error) => warn!(%error, "skipping malformed sync event"),
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn redaction(id: usize) -> Raw<AnySyncTimelineEvent> {
        Raw::new(&json!({
            "type": REDACTION,
            "event_id": format!("$redaction{id}"),
            "sender": "@alice:example.org",
            "origin_server_ts": 1,
            "content": { "redacts": format!("$target{id}") },
        }))
        .expect("serializable")
        .cast_unchecked()
    }

    #[test]
    fn remembered_redactions_are_bounded() {
        let mut index = CalendarIndex::new();
        for id in 0..MAX_REMEMBERED_REDACTIONS + 500 {
            assert!(!index.apply_raw_sync(&redaction(id)).expect("valid event"));
        }
        assert_eq!(index.redacted.len(), MAX_REMEMBERED_REDACTIONS);
        // The newest redaction is always remembered.
        let newest = format!("$target{}", MAX_REMEMBERED_REDACTIONS + 499);
        assert!(index.redacted.iter().any(|id| id.as_str() == newest));
    }
}
