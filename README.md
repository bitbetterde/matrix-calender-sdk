# matrix-calendar-sdk

A Rust SDK for federated calendars over Matrix, implementing
[MSC4496](https://github.com/matrix-org/matrix-spec-proposals/pull/4496)
("Calendar Events, Invites, and Availability in Matrix") on top of
[matrix-sdk](https://crates.io/crates/matrix-sdk).

MSC4496 maps calendaring onto Matrix as follows:

| Concept             | Matrix representation                                               |
| ------------------- | ------------------------------------------------------------------- |
| Calendar            | Room with room type `m.calendar` (unstable: `org.matrix.msc4496.calendar`) |
| Calendar metadata   | State event `m.calendar.info` (colour, home timezone)               |
| Calendar event      | Timeline event `m.calendar.event`, edited via `m.replace`           |
| Recurrence          | Structured RRULE JSON on the event, overrides via `m.calendar.override` relation |
| Invite / RSVP       | Timeline events `m.calendar.invite` and `m.calendar.rsvp` in any shared room |
| Availability        | State events for policy and working hours, plus a query endpoint    |

Sharing a calendar between two users is therefore just inviting the other user to the
calendar room, and accepting a shared calendar is joining it. Federation, membership, power
levels, and end-to-end encryption all come from Matrix; the SDK maps calendar roles onto power
levels (see [Permissions](#permissions)).

Note the two unrelated meanings of "invite": `m.calendar.invite` / `m.calendar.rsvp` are
invitations to a single calendar **event** ("join my meeting", the `scheduling` module), while
inviting someone to the calendar **room** shares the whole calendar (`Calendar::invite`,
`CalendarInvitation`).

The SDK sends the unstable identifiers from the MSC and accepts both the stable and the
unstable identifiers when reading.

## Crate layout

- `events` – typed, serde-backed content types for every event in the MSC, round-tripping
  the MSC's JSON examples.
- `recurrence` – timezone-aware expansion of recurring events into concrete occurrences,
  honouring exdates, rdates, and overrides. The series start is always the first occurrence
  (RFC 8984) and instances keep the exact duration of the original (RFC 5545).
- `calendar` – `Calendar` (a calendar room), `CalendarInvitation` (a calendar someone shared
  with you, not accepted yet) and `CalendarIndex` (an in-memory view built from the room
  timeline with edits and overrides applied).
- `scheduling` – sending invites and RSVPs, and aggregating responses per event, keyed by
  `(organiser, uid)`; invites that claim someone else as organiser are ignored. The
  `organizer` field is optional and defaults to the sender.
- `availability` – busy/free computation, availability policy and working-hours state,
  and the client-server availability query.
- `permissions` – calendar roles (reader, editor, owner) and the power-level scheme through
  which the homeserver enforces them.

See `docs/DESIGN.md` for the architecture and `docs/MSC4496-matrix-caldav.md` for the
proposal text this crate implements.

## Permissions

MSC4496 leaves access control to the room ("room ACLs govern access"). The SDK maps three
roles onto Matrix power levels, so the **homeserver** enforces them, for every client:

| Role     | Power level | May                                                                                   |
| -------- | ----------- | ------------------------------------------------------------------------------------- |
| `Reader` | 0           | read the calendar, RSVP (configurable), react                                          |
| `Editor` | 50          | also create calendar events, and edit or cancel **their own** events                   |
| `Owner`  | 100         | also change calendar info and availability, invite and remove members, change roles, redact other users' events |

Sharing read-only or read-write is one call; the role is set before the invite is sent:

```rust
calendar.invite_reader(&bob).await?;                       // read-only
calendar.invite_editor(&carol).await?;                     // read-write
calendar.set_role(&bob, CalendarRole::Editor).await?;      // change a role later
let mine = calendar.my_role().await?;                      // Reader / Editor / Owner
```

The receiving side is `calendar::list_calendar_invitations`, which returns the pending
invitations to calendar rooms (invites to other rooms are skipped, so this is safe on a client
that is also used for chat):

```rust
for invitation in list_calendar_invitations(&client) {
    println!("{:?} shared {:?}", invitation.inviter().await, invitation.name().await);
    let shared = invitation.accept().await?;      // join the calendar room
    // or: invitation.decline().await?;           // reject the invite
    shared.leave().await?;                        // stop sharing it again later
}
```

A pending invitation only exposes what the homeserver put into the **stripped invite state**;
a non-member may not read the room state. Synapse sends `m.room.create` (so the room type, and
with it calendar detection, is reliable), `m.room.name`, `m.room.join_rules`,
`m.room.encryption` and the `m.room.member` events, but *not* the `calendar_info` or
`m.room.power_levels` state events. So `name()`, `inviter()` and `is_encrypted()` work, while:

- `color()` (and `info()`) are `None` until the calendar has been joined — show a placeholder
  colour and read `Calendar::info` after accepting;
- `role()` is informational and falls back to `Reader` (the SDK's default role) when the power
  levels are missing, so even an invitation as an editor reads as `Reader` beforehand. The role
  the homeserver enforces is what `Calendar::my_role` reports after accepting.

`accept` returns a `Calendar` that is usable right away (`my_role`, `members_with_roles` and
`load` fall back to the homeserver), but `Calendar::info` and `list_calendars` only see it once
the join has come back through `/sync` — the same caveat as `Calendar::create`.
`decline` leaves the room, which `matrix-sdk` also forgets because it was an invite;
`Calendar::leave` leaves *and* forgets a joined calendar. Neither deletes the calendar: Matrix
has no "delete room", the room keeps existing for its other members, and the SDK does not
refuse to let the last owner leave (make someone else an owner first if the calendar should
stay manageable).

Roles can also be given at creation (`CreateCalendarOptions::members`), and
`members_with_roles`, `role_of`, `permissions`, `apply_permissions`, `set_default_role` and
`remove_member` cover the rest. Roles are derived from what the power levels *allow*
(owner = may change power levels, editor = may send calendar events), so calendars created by
other clients are classified correctly; `apply_permissions` brings them under the SDK scheme.
Write operations of a reader fail early with `Error::PermissionDenied` naming the missing
permission, and the server rejects them anyway.

Caveats:

- **Editors can only edit their own events.** Matrix only accepts `m.replace` edits from the
  original sender, so nobody, not even an owner, can change another user's event. Owners can
  redact (delete) other users' events.
- **Behaviour change:** members without an explicit role are now **readers** (least privilege).
  Previously every member of a calendar created by the SDK could write. Invite writers as
  editors (`invite_editor`, `CreateCalendarOptions::members`), or set
  `CreateCalendarOptions::permissions` to `CalendarPermissions::new(CalendarRole::Editor)`.
  Existing calendars keep their power levels until `apply_permissions` is called.
- In encrypted calendars the server only sees `m.room.encrypted`, so readers cannot send RSVPs
  into the calendar room there (they can RSVP in a DM).
- In room version 12 and later, room creators have unlimited power: they are always owners and
  their role cannot be changed.

## Status

Early development. The MSC itself is a draft with no accepted implementations yet, and
the availability query endpoints need homeserver support that does not exist today.

## Building

```sh
cargo build
cargo test
cargo clippy --all-targets -- -D warnings
```

## End-to-end tests

`tests/e2e.rs` runs the SDK against a real homeserver: calendar creation and discovery,
sharing, accepting and declining invitations and leaving a calendar, edits and cancellation,
overrides, redactions, the live index, encrypted calendars, event invites and RSVPs, spoofing
attempts, availability, events sent with the stable identifiers,
and pagination of a large calendar. Without `MATRIX_E2E_HOMESERVER` every test prints
"skipped" and passes, so a plain `cargo test` needs no server.

They need a homeserver reachable over plain HTTP with password login and three existing users,
`alice`, `bob` and `mallory`, ideally with rate limits lifted. Each test logs in new devices,
creates its own uniquely named rooms, and leaves those rooms and logs out at the end, so the
tests can run in parallel and repeatedly against the same server.

| Variable                     | Meaning                                                    | Default         |
| ---------------------------- | ---------------------------------------------------------- | --------------- |
| `MATRIX_E2E_HOMESERVER`      | Homeserver base URL (used directly, no discovery); required | –               |
| `MATRIX_E2E_PASSWORD_PREFIX` | The password of user `X` is this prefix followed by `X`     | `calendar-e2e-` |

```sh
MATRIX_E2E_HOMESERVER=http://matrix.ess.localhost cargo test --test e2e -- --nocapture
# or, with the defaults above and a reachability check:
scripts/e2e.sh                     # all tests
scripts/e2e.sh encrypted_calendar  # one test
```

## License

Apache-2.0
