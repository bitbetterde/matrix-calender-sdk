//! A calendar SDK for Matrix implementing
//! [MSC4496](https://github.com/matrix-org/matrix-spec-proposals/pull/4496)
//! ("Calendar Events, Invites, and Availability in Matrix") on top of [`matrix_sdk`].
//!
//! # Overview
//!
//! - [`events`]: typed event contents for calendar rooms, calendar events (including
//!   recurrence, overrides and edits), invites, RSVPs and availability, whose JSON matches the
//!   MSC wire format exactly.
//! - [`ids`]: the stable and unstable identifiers from the MSC, plus helpers that accept both.
//! - [`recurrence`]: timezone-aware recurrence expansion.
//! - [`calendar`]: a high-level API over a calendar room, pending invitations to calendars
//!   shared with the current user, an index of its events with occurrence and busy-time
//!   queries, and a live-updating variant fed by sync.
//! - [`scheduling`]: invites and RSVPs.
//! - [`availability`]: busy/free computation and availability policies.
//! - [`permissions`]: calendar roles (reader, editor, owner) enforced by the homeserver
//!   through power levels.
//!
//! While MSC4496 is unstable, the SDK sends the unstable identifiers
//! (`org.matrix.msc4496.*`) and accepts the stable `m.calendar.*` identifiers on input.
//!
//! Most users will want to start with `use matrix_calendar_sdk::prelude::*;`.

pub mod availability;
pub mod calendar;
pub mod error;
pub mod events;
pub mod ids;
pub mod key;
pub mod permissions;
pub mod recurrence;
pub mod scheduling;

pub use error::{Error, Result};
pub use key::EventKey;

/// Commonly used types, for glob import.
pub mod prelude {
    pub use crate::{
        calendar::{
            Calendar, CalendarIndex, CalendarInvitation, CalendarOccurrence, CreateCalendarOptions,
            EventKey, IndexChange, IndexedEvent, IndexedOverride, LiveCalendar, LoadOptions,
            list_calendar_invitations, list_calendars,
        },
        error::{Error, Result},
        events::{
            AvailabilityPolicyEventContent, AvailabilityRequest, AvailabilityResponse,
            AvailabilityWindowEventContent, BusyInterval, BusyType, ByDay, CalendarEvent,
            CalendarEventEventContent, CalendarEventEventContentWithoutRelation,
            CalendarEventRelation, CalendarInfo, CalendarInfoEventContent, CalendarInvite,
            CalendarInviteEventContent, CalendarOverride, CalendarRsvp, CalendarRsvpEventContent,
            Conference, EventTime, Frequency, Grant, InviteEventSummary, InviteMethod, Location,
            LocationType, PartStat, RRule, Recurrence, Status, Transparency, Until, Weekday,
            Window,
        },
        permissions::{
            CalendarMember, CalendarMembership, CalendarPermissions, CalendarPermissionsSummary,
            CalendarRole,
        },
        recurrence::{ExpandOptions, Occurrence},
        scheduling::{AttendeeStatus, InviteRecord, RsvpRecord, SchedulingIndex},
    };
}
