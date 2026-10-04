# matrix-calendar-sdk design

A Rust SDK that implements [MSC4496](./MSC4496-matrix-caldav.md) ("Calendar Events, Invites,
and Availability in Matrix") on top of `matrix-sdk` 0.19. It gives clients a typed API for
calendar rooms, calendar events, recurrence expansion, invites/RSVPs and availability
without re-implementing Matrix plumbing.

Read the MSC in `docs/MSC4496-matrix-caldav.md` before touching any module. The MSC is the
source of truth for the JSON wire format. Where the MSC is silent, prefer JSCalendar
(RFC 8984) semantics, then iCalendar (RFC 5545).

## Ground rules

- Rust edition 2024, stable toolchain. `cargo build`, `cargo test`, and
  `cargo clippy --all-targets -- -D warnings` must all pass before a task is considered done.
- Use `matrix_sdk::ruma` for all Matrix types. The crate also depends on `ruma` directly only
  to enable the `unstable-msc1767` and `unstable-msc3488` features; it is the same version as
  the one re-exported by `matrix-sdk`, so both paths refer to identical types.
- Event types use the **unstable identifiers** from the MSC's "Unstable prefix" table when
  sending (for example `org.matrix.msc4496.calendar_event`), and accept the stable identifier
  (`m.calendar.event`) as an alias when receiving. ruma's `#[ruma_event(type = "...", alias = "...")]`
  supports this.
- Every event content struct must round-trip the JSON examples from the MSC exactly
  (field names, nesting, nulls vs. omitted). Add unit tests that deserialize the MSC
  examples verbatim and re-serialize them.
- Content-block layout: the MSC follows MSC1767 extensible events. The event `content` holds
  an `m.text` block (fallback text) and a block named after the event type that carries the
  structured payload. The `m.text` block is required by the MSC when sending in unencrypted
  rooms and must be omitted from the plaintext when the room is encrypted (the SDK sends it
  inside the encrypted payload, which is what `matrix-sdk` does automatically for the whole
  content, so in practice it is always included in the content struct and encryption hides it).
- No `unwrap()`/`expect()` in library code; return `crate::Error`.
- Doc comments on every public item. Keep modules small and focused.
- Do not add dependencies beyond those in `Cargo.toml` without a good reason. If you add one,
  say why in the task report.

## Module layout

```
src/
  lib.rs                 crate docs, module declarations, re-exports (prelude)
  error.rs               crate::Error (thiserror) and crate::Result
  key.rs                 EventKey { sender (organiser), uid }, shared by calendar and scheduling
  ids.rs                 constants for all stable + unstable event type strings, room type,
                         rel_type, and helpers like `is_calendar_event_type(&str)`
  events/
    mod.rs
    text.rs              helpers to build the MSC1767 `m.text` fallback block
    datetime.rs          EventTime { Timed { utc, localtime, timezone } | AllDay { date } },
                         validation (IANA tz, consistency between utc/localtime/timezone),
                         conversion helpers to/from chrono + chrono-tz
    location.rs          Location { Physical | Virtual } with name + optional uri
    conference.rs        Conference block (type m.rtc, application, room_id, slot_id, via, label)
    recurrence.rs        serde types only: Recurrence { rrule: RRule, exdates }, RRule JSON
                         object with freq/interval/count/until/byday/bymonthday/bymonth/
                         byhour/byminute/bysecond/byyearday/byweekno/bysetpos/wkst
    calendar_info.rs     state event `org.matrix.msc4496.calendar_info` (state_key "")
    calendar_event.rs    message-like event `org.matrix.msc4496.calendar_event`, incl.
                         `recurrence_id` for overrides, the `m.calendar.override` relation,
                         and `m.replace` edit support (`m.new_content`)
    invite.rs            message-like event `org.matrix.msc4496.calendar_invite`
    rsvp.rs              message-like event `org.matrix.msc4496.calendar_rsvp` with
                         `m.reference` relation to the invite
    availability.rs      state events `availability_policy` and `availability_window`
  recurrence.rs          expansion: given a CalendarEventContent plus its override events,
                         produce concrete occurrences in a UTC range, timezone-aware,
                         honouring exdates and overrides (uses the `rrule` crate)
  calendar.rs            high-level room API: Calendar wraps matrix_sdk::Room
                         (create calendar room, read/update info, list events via
                         timeline/pagination, create/edit/cancel events, send overrides,
                         leave), plus CalendarInvitation for calendars shared with the
                         current user but not accepted yet (accept / decline)
  scheduling.rs          invites and RSVPs: send invite, respond, aggregate responses per
                         (organiser, uid)
  availability.rs        client-side busy/free computation from events, read/write of the
                         availability policy and window state events
  permissions.rs         calendar roles, the power-level scheme, role derivation, and the
                         invite / set_role / apply_permissions operations behind `Calendar`
tests/                   serde fixtures from the MSC, recurrence tests, and integration tests
                         against a mocked homeserver (matrix-sdk `testing` feature + wiremock)
examples/                small runnable examples
```

## Key design decisions

- **Timeline events, not state.** Per MSC4496 §2 calendar events are timeline events.
  Reading a calendar therefore means paginating the room timeline, collecting
  `calendar_event` events, applying `m.replace` edits (latest wins, same sender only) and
  grouping overrides by `uid`. `calendar.rs` exposes this as an in-memory `CalendarIndex`
  built from the timeline; later versions can persist it.
- **UID scope.** UIDs are only unique per organiser (`sender`). Index keys are `(sender, uid)`
  (`EventKey`). The scheduling index uses the same key with the invite's organiser: the
  `calendar_event.organizer` it names, or its sender if it names none. `REQUEST`/`CANCEL`
  invites are only accepted from that organiser (others are dropped with a warning, so a room
  member cannot hijack or cancel someone else's event by re-using its `uid`); a `COUNTER` must
  name the organiser it is filed under; RSVPs inherit the key of the invite they reference, and
  redacting an invite drops its RSVPs.
- **Organiser.** `organizer` is optional on the wire (the MSC §5.1 payload omits it) and is
  omitted when absent. A missing organiser means the sender (`IndexedEvent::organizer`,
  `InviteRecord::organizer`).
- **Edits.** Only the original sender may edit. `Calendar::update_event` sends an
  `m.replace` relation with `m.new_content` carrying the full new content. Cancellation is an
  edit that sets `status: cancelled`.
- **Overrides.** An override is a separate `calendar_event` with `recurrence_id` and an
  `m.relates_to` of rel_type `org.matrix.msc4496.calendar_override` pointing at the series
  event. Overrides may themselves be edited via `m.replace`.
- **Timezones.** `EventTime::Timed` stores `utc`, `localtime`, and an IANA `timezone`. The
  SDK validates that `localtime` in `timezone` equals `utc` at construction, and recurrence
  expansion runs in the named timezone, never in UTC (MSC "Potential issues").
- **Date range.** Years are limited to 1..=9999: deserialization of `EventTime` (and of UTC
  timestamps) rejects anything else, and all date arithmetic in library code is checked or
  saturating, so remote data cannot cause a panic.
- **Recurrence semantics.** The series start (`DTSTART`) is always the first instance and
  counts toward `COUNT`, even if it does not match the rule (RFC 8984 §4.3.3); the MSC example
  (a Wednesday start with `BYDAY=MO`) starts on that Wednesday. Generated instances of timed
  events have the master's exact duration (RFC 5545 §3.3.6, `end.utc - start.utc`), so their
  local end time can shift across a DST transition; all-day instances last whole days.
- **Busy time** must be computed from expanded occurrences
  (`availability::compute_busy_from_occurrences`, or `CalendarIndex::busy`);
  `compute_busy` skips series masters with a warning.
- **Forward compatibility.** String enums (`Status`, `Transparency`, `PartStat`,
  `InviteMethod`, `LocationType`, `BusyType`, `Frequency`) are `#[non_exhaustive]` and keep
  unknown values in an `Other(String)` variant so that they round-trip.
- **Sharing is room membership.** Sharing a calendar is inviting someone to the calendar room
  (`Calendar::invite`), accepting it is joining (`CalendarInvitation::accept`), declining and
  un-sharing are leaving (`CalendarInvitation::decline`, `Calendar::leave`). This is distinct
  from `m.calendar.invite`/`m.calendar.rsvp`, which invite to a single *event* and live in
  `scheduling`; the naming keeps them apart (`CalendarInvitation` vs `CalendarInvite`).
  `list_calendar_invitations` filters `Client::invited_rooms` by room type, so invites to chat
  rooms on the same client are ignored. Neither leave operation deletes the calendar — Matrix
  has no "delete room" — and the last owner is allowed to leave (unlike `set_role`, which
  refuses to demote the last owner while others are still in the room); `Calendar::leave` also
  forgets the room, which `matrix-sdk` does by itself for invites but not for joined rooms.
- **What an invitation can know.** Only the stripped invite state, which a homeserver composes
  from a fixed list of event types (Synapse's `room_prejoin_state_types`: create, join rules,
  name, topic, avatar, canonical alias, encryption, plus the member events). The create event
  makes the room type reliable, so detecting calendar invites and reading the name, inviter and
  encryption works; the `calendar_info` and `m.room.power_levels` state events are *not*
  included and cannot be fetched (`/state` is forbidden for non-members), so
  `CalendarInvitation::color` is `None` and `CalendarInvitation::role` falls back to
  `CalendarRole::Reader`, the SDK's default role (least privilege). The role before joining is
  therefore informational; the homeserver decides, and `Calendar::my_role` after accepting is
  authoritative. Reading the stripped power levels is implemented anyway
  (`permissions::local_levels`), so it works on servers that do include them.
- **Encryption.** Nothing special is needed; `matrix-sdk` encrypts the full content in E2EE
  rooms. The optional feature `experimental-encrypted-state-events` of matrix-sdk is not used.
  `Room::encryption_state` stays `Unknown` for rooms that have only been invited to, so
  `CalendarInvitation::is_encrypted` reads the stripped `m.room.encryption` event directly.
- **Availability endpoints** (§4.5, §4.6) need server support that does not exist yet. The
  SDK ships the request/response types and a client-side busy computation; the HTTP call is
  implemented against the unstable path and will simply fail against servers without support.

## Permissions

MSC4496 defines no access-control mechanism of its own ("room ACLs govern access"), so the SDK
uses Matrix power levels, which the homeserver enforces for every client. Three roles map to
power levels: `Reader` 0, `Editor` 50, `Owner` 100.

### Power-level scheme

`permissions::power_levels_for(&CalendarPermissions, &AuthorizationRules)` produces the
`m.room.power_levels` content. `CalendarPermissions` has `default_role` (`Reader` by default,
`Editor` allowed, `Owner` rejected), `readers_can_rsvp` (default `true`) and
`editors_can_invite` (default `false`).

| Key / event type                                                        | Level                         |
| ----------------------------------------------------------------------- | ----------------------------- |
| `users_default`                                                         | default role: 0 or 50         |
| `events_default` (chat, and everything in encrypted rooms)              | 50                            |
| `state_default`, `ban`, `kick`, `redact`                                | 100                           |
| `invite`                                                                | 50 if `editors_can_invite`, else 100 |
| `calendar_event`, `calendar_invite` (stable + unstable)                 | 50                            |
| `calendar_rsvp` (stable + unstable)                                     | 0 if `readers_can_rsvp`, else 50 |
| `m.reaction`, `m.room.redaction` (own events; others' need `redact`)    | 0                             |
| `calendar_info`, `availability_policy`, `availability_window` (stable + unstable) | 100                 |
| `m.room.name`, `topic`, `avatar`, `power_levels`, `history_visibility`, `canonical_alias`, `encryption`, `server_acl` | 100 |
| `m.room.tombstone`                                                      | 100; 150 in room versions with privileged creators (v12+), Matrix's default there |

- **Creation.** `Calendar::create` sends the scheme as `power_level_content_override`
  with every key spelled out: Synapse merges the override's top-level keys into its own
  defaults (a shallow merge, so `events` replaces the default map entirely, and an omitted key
  keeps the preset's value, for example `invite: 0` of `private_chat`). The `users` map is only
  sent when someone gets an explicit role (`CreateCalendarOptions::members`, or invitees with
  the `trusted_private_chat` preset). Then the room version matters: before version 12 the
  creator must be in `users` (Synapse rejects an override without them), from 12 on creators
  must not be (rejected as well). The SDK uses `CreateCalendarOptions::room_version` or the
  server's default version from `/capabilities`, and pins it in the request.
- **Updates** (`invite`, `set_role`, `apply_permissions`, `set_default_role`,
  `remove_member`) read the current power levels from the homeserver (`GET
  /state/m.room.power_levels/`), not from the local store, modify the raw JSON (so keys ruma
  does not know, such as Synapse's `historical`, survive) and send it back. Consecutive calls
  therefore do not overwrite each other even before the client has synced.
  `apply_permissions` overwrites the scheme's keys and `events` entries and keeps per-user
  levels, other `events` entries and unknown keys; `apply_permissions_normalizing_users` also
  maps user levels to the nearest role level. Changes that would remove the caller's own owner
  role are refused.
- **Invites** set the user's level first (power levels may name users who are not in the room)
  and then invite, so the role is in place before the user can join. The membership is read
  from the server; already joined or invited users only get their role adjusted (re-inviting a
  joined user would fail with `M_FORBIDDEN`). Explicit roles are pinned, so a later change of
  the default role does not affect them.

### Role derivation

Roles are derived from **capabilities**, not raw numbers, so rooms created by other clients
or with Matrix's default power levels are classified correctly:

1. `Owner` if the user may send `m.room.power_levels`;
2. else `Editor` if the user may send `org.matrix.msc4496.calendar_event`;
3. else `Reader`.

With Matrix's default power levels (`events_default` 0) every member is therefore an editor and
the creator an owner. Room creators in room version 12+ have unlimited power
(`UserPowerLevel::Infinite`): they are always owners, never appear in `users`, and
`set_role` refuses to change them. `set_role` also refuses to demote the last joined owner,
and fails with `Error::InvalidPermissions` when the room does not follow the scheme so that the
role's level would not give the requested role (call `apply_permissions` first).

`role_of`, `my_role`, `can_write`, `permissions` and `members_with_roles` read the locally
synced power levels and fall back to the homeserver when they are not synced yet (right after
creating or joining a room). `CalendarInvitation::role` cannot do either (see "What an
invitation can know" above) and reads the stripped state, falling back to `Reader`. The client-side guards on `create_event`, `update_event`, `cancel_event`,
`create_override(_for)`, `cancel_occurrence`, `set_info`, `availability::set_policy` and
`set_window` use only the local state (skipping the check if it is unknown) and return
`Error::PermissionDenied` naming the missing permission; the homeserver remains the authority.

### Limits

- Matrix only accepts edits from the original sender, so editors (and owners) can only edit
  their own events; owners can redact other users' events.
- In encrypted rooms every event is `m.room.encrypted` to the server, governed by
  `events_default` (50): the server still enforces read-only access, but readers cannot RSVP
  inside an encrypted calendar.
