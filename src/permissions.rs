//! Calendar roles and the power-level scheme that enforces them.
//!
//! MSC4496 leaves access control to the room: "room ACLs govern access". This module maps
//! three calendar roles onto Matrix power levels, so that the **homeserver** enforces them:
//!
//! | Role                       | Power level | May                                                         |
//! | -------------------------- | ----------- | ----------------------------------------------------------- |
//! | [`CalendarRole::Reader`]   | 0           | read the calendar, RSVP (unless disabled), react            |
//! | [`CalendarRole::Editor`]   | 50          | also create calendar events, edit and cancel **their own**  |
//! | [`CalendarRole::Owner`]    | 100         | also change calendar info and availability, manage members and roles, redact other users' events |
//!
//! **Editors can only edit their own events.** Matrix has no way to let one user edit another
//! user's event: an `m.replace` edit is only valid from the original sender, and every client
//! (including this SDK's [`CalendarIndex`](crate::calendar::CalendarIndex)) ignores edits from
//! anyone else. Owners can redact (delete) other users' events, but not change them.
//!
//! [`power_levels_for`] produces the scheme; see the crate's `docs/DESIGN.md` for the full
//! table. Roles are read back from the room's actual power levels by **capability**, not by
//! number ([`role_from_power_levels`]), so rooms created by other clients, or with Matrix's
//! default power levels, are classified correctly:
//!
//! - **Owner** if the user may send `m.room.power_levels`,
//! - else **Editor** if the user may send calendar events,
//! - else **Reader**.
//!
//! Room creators in room version 12 and later have unlimited power: they are always Owners,
//! never appear in the `users` map, and their role cannot be changed.
//!
//! In **encrypted** calendars every event is sent as `m.room.encrypted`, so the server only
//! sees that type: it is governed by `events_default` (50), and readers therefore cannot send
//! RSVPs into an encrypted calendar (they can still RSVP in a DM or another room).
//!
//! The high-level API is on [`Calendar`](crate::calendar::Calendar): `invite`, `set_role`,
//! `role_of`, `members_with_roles`, `permissions`, `apply_permissions`, ...

use std::{cmp::Reverse, collections::BTreeMap, fmt};

use matrix_sdk::{
    Client, HttpError, Room, RoomMemberships,
    deserialized_responses::RawAnySyncOrStrippedState,
    ruma::{
        Int, OwnedUserId, RoomVersionId, UserId,
        api::client::{
            room::create_room::RoomPowerLevelsContentOverride,
            state::{get_state_event_for_key, get_state_events},
        },
        events::{
            MessageLikeEventType, StateEventType,
            room::{
                member::MembershipState,
                power_levels::{
                    RoomPowerLevels, RoomPowerLevelsEventContent, RoomPowerLevelsSource,
                    UserPowerLevel,
                },
            },
        },
        room_version_rules::{AuthorizationRules, RoomVersionRules},
        serde::Raw,
    },
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tracing::{debug, warn};

use crate::{Error, Result, ids};

/// The `m.room.power_levels` state event type.
const POWER_LEVELS: &str = "m.room.power_levels";

// ---------------------------------------------------------------------------------------------
// Roles and permissions
// ---------------------------------------------------------------------------------------------

/// What a member may do in a calendar.
///
/// Roles are ordered: `Reader < Editor < Owner`. Serialized in lowercase (`"reader"`, ...).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum CalendarRole {
    /// Can read the calendar (join, sync, load the index) and RSVP (if
    /// [`CalendarPermissions::readers_can_rsvp`]), but cannot create, edit or cancel calendar
    /// events, and cannot send chat messages into the calendar room.
    Reader,
    /// Can also create calendar events and edit or cancel **their own** events. Matrix does
    /// not let anyone edit another user's event.
    Editor,
    /// Can also change the calendar info and availability state, invite and remove members,
    /// change roles, and redact other users' events.
    Owner,
}

impl CalendarRole {
    /// All roles, from least to most privileged.
    pub const ALL: [CalendarRole; 3] = [Self::Reader, Self::Editor, Self::Owner];

    /// The power level the SDK gives users with this role: Reader 0, Editor 50, Owner 100.
    pub const fn power_level(self) -> i64 {
        match self {
            Self::Reader => 0,
            Self::Editor => 50,
            Self::Owner => 100,
        }
    }

    /// Whether this role may send calendar events (Editor or Owner).
    pub fn can_write(self) -> bool {
        self >= Self::Editor
    }

    /// Whether this role may manage the calendar: its info, members and roles (Owner).
    pub fn can_manage(self) -> bool {
        self == Self::Owner
    }

    /// The role whose [`power_level`](Self::power_level) is nearest to `level`; ties go to the
    /// less privileged role.
    pub fn nearest(level: i64) -> Self {
        if level > 75 {
            Self::Owner
        } else if level > 25 {
            Self::Editor
        } else {
            Self::Reader
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Reader => "reader",
            Self::Editor => "editor",
            Self::Owner => "owner",
        }
    }
}

impl fmt::Display for CalendarRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The power-level scheme the SDK applies to calendar rooms (see [`power_levels_for`]).
///
/// The default is least privilege: members get [`CalendarRole::Reader`] unless they are
/// given another role explicitly, readers may RSVP, and only owners may invite.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub struct CalendarPermissions {
    /// The role of members without an explicit role (the `users_default` power level).
    /// [`CalendarRole::Reader`] or [`CalendarRole::Editor`]; `Owner` is rejected.
    pub default_role: CalendarRole,
    /// Whether readers may send RSVPs (`calendar_rsvp` events) into the calendar room.
    /// Defaults to `true`. Has no effect in encrypted calendars, see the module docs.
    pub readers_can_rsvp: bool,
    /// Whether editors may invite users (who then get the default role). Defaults to `false`.
    pub editors_can_invite: bool,
}

impl CalendarPermissions {
    /// Permissions with the given default role, readers allowed to RSVP and only owners
    /// allowed to invite.
    pub fn new(default_role: CalendarRole) -> Self {
        Self {
            default_role,
            readers_can_rsvp: true,
            editors_can_invite: false,
        }
    }

    /// Checks that the permissions can be applied: the default role must not be `Owner`.
    pub fn validate(&self) -> Result<()> {
        if self.default_role.can_manage() {
            return Err(Error::InvalidPermissions(
                "the default role cannot be owner: every member could then take over the \
                 calendar; give owners their role explicitly"
                    .into(),
            ));
        }
        Ok(())
    }
}

impl Default for CalendarPermissions {
    fn default() -> Self {
        Self::new(CalendarRole::Reader)
    }
}

/// The permission settings of a calendar room as read from its power levels, returned by
/// [`Calendar::permissions`](crate::calendar::Calendar::permissions).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub struct CalendarPermissionsSummary {
    /// The role of members without an explicit power level (derived from `users_default`).
    /// May be [`CalendarRole::Owner`] in rooms that do not follow the SDK scheme.
    pub default_role: CalendarRole,
    /// Whether a member with power level 0 may send RSVPs.
    pub readers_can_rsvp: bool,
    /// Whether a member with the editor power level (50) may invite.
    pub editors_can_invite: bool,
    /// Whether the room follows the SDK scheme exactly: every level that
    /// [`power_levels_for`] sets has the scheme's value for the settings above, and every
    /// explicit user level is a role level (0, 50 or 100). Event types the scheme does not
    /// mention are ignored.
    pub is_standard: bool,
}

impl CalendarPermissionsSummary {
    /// The settings as [`CalendarPermissions`], for example to change one of them and apply
    /// the result with [`Calendar::apply_permissions`](crate::calendar::Calendar::apply_permissions).
    /// A default role of `Owner` is mapped to `Editor`.
    pub fn as_permissions(&self) -> CalendarPermissions {
        CalendarPermissions {
            default_role: self.default_role.min(CalendarRole::Editor),
            readers_can_rsvp: self.readers_can_rsvp,
            editors_can_invite: self.editors_can_invite,
        }
    }
}

/// The membership of a [`CalendarMember`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum CalendarMembership {
    /// The user has joined the calendar room.
    Joined,
    /// The user has been invited but has not joined yet.
    Invited,
}

/// A member of a calendar with their role, returned by
/// [`Calendar::members_with_roles`](crate::calendar::Calendar::members_with_roles).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct CalendarMember {
    /// The member's user ID.
    pub user_id: OwnedUserId,
    /// The member's display name in the room, if any.
    pub display_name: Option<String>,
    /// Whether the member has joined or is only invited.
    pub membership: CalendarMembership,
    /// The member's role, derived from the room's power levels.
    pub role: CalendarRole,
}

// ---------------------------------------------------------------------------------------------
// The scheme
// ---------------------------------------------------------------------------------------------

/// Level needed for message-like events without an entry in `events` (chat messages, and
/// every event in an encrypted room).
const EVENTS_DEFAULT: i64 = 50;
/// Level needed for state events without an entry in `events`.
const STATE_DEFAULT: i64 = 100;
/// Level needed to ban, kick, and redact other users' events.
const MODERATION: i64 = 100;
/// Level needed for `m.room.tombstone` (room upgrades) in room versions with privileged
/// creators, matching Matrix's default there (only creators can upgrade).
const TOMBSTONE_PRIVILEGED_CREATORS: i64 = 150;

/// The top-level levels of the scheme, as `(key, level)`.
fn scheme_top_level(perms: &CalendarPermissions) -> [(&'static str, i64); 7] {
    let invite = if perms.editors_can_invite {
        CalendarRole::Editor
    } else {
        CalendarRole::Owner
    };
    [
        ("users_default", perms.default_role.power_level()),
        ("events_default", EVENTS_DEFAULT),
        ("state_default", STATE_DEFAULT),
        ("ban", MODERATION),
        ("kick", MODERATION),
        ("redact", MODERATION),
        ("invite", invite.power_level()),
    ]
}

/// The `events` entries of the scheme, as `(event type, level)`.
fn scheme_events(
    perms: &CalendarPermissions,
    privileged_creators: bool,
) -> Vec<(&'static str, i64)> {
    let editor = CalendarRole::Editor.power_level();
    let owner = CalendarRole::Owner.power_level();
    let rsvp = if perms.readers_can_rsvp {
        CalendarRole::Reader.power_level()
    } else {
        editor
    };
    let tombstone = if privileged_creators {
        TOMBSTONE_PRIVILEGED_CREATORS
    } else {
        owner
    };
    vec![
        // Calendar timeline events.
        (ids::CALENDAR_EVENT_UNSTABLE, editor),
        (ids::CALENDAR_EVENT, editor),
        (ids::CALENDAR_INVITE_UNSTABLE, editor),
        (ids::CALENDAR_INVITE, editor),
        (ids::CALENDAR_RSVP_UNSTABLE, rsvp),
        (ids::CALENDAR_RSVP, rsvp),
        // Everyone may react, and redact their own events (redacting other users' events is
        // governed by `redact`).
        ("m.reaction", 0),
        ("m.room.redaction", 0),
        // Calendar state.
        (ids::CALENDAR_INFO_UNSTABLE, owner),
        (ids::CALENDAR_INFO, owner),
        (ids::AVAILABILITY_POLICY_UNSTABLE, owner),
        (ids::AVAILABILITY_POLICY, owner),
        (ids::AVAILABILITY_WINDOW_UNSTABLE, owner),
        (ids::AVAILABILITY_WINDOW, owner),
        // Room state.
        ("m.room.name", owner),
        ("m.room.topic", owner),
        ("m.room.avatar", owner),
        ("m.room.power_levels", owner),
        ("m.room.history_visibility", owner),
        ("m.room.canonical_alias", owner),
        ("m.room.encryption", owner),
        ("m.room.server_acl", owner),
        ("m.room.tombstone", tombstone),
    ]
}

/// Writes the scheme's levels into the power-levels content `content`, keeping its `users`,
/// `notifications`, unknown keys and `events` entries for types the scheme does not mention.
fn apply_scheme(content: &mut Map<String, Value>, perms: &CalendarPermissions, privileged: bool) {
    for (key, level) in scheme_top_level(perms) {
        content.insert(key.to_owned(), json!(level));
    }
    let events = content
        .entry("events")
        .or_insert_with(|| Value::Object(Map::new()));
    if !events.is_object() {
        *events = Value::Object(Map::new());
    }
    if let Value::Object(events) = events {
        for (event_type, level) in scheme_events(perms, privileged) {
            events.insert(event_type.to_owned(), json!(level));
        }
    }
}

/// The scheme as a complete power-levels content with every level spelled out and an empty
/// `users` map.
fn scheme_json(perms: &CalendarPermissions, privileged_creators: bool) -> Map<String, Value> {
    let mut content = Map::new();
    apply_scheme(&mut content, perms, privileged_creators);
    content
}

/// The `m.room.power_levels` content of the SDK's scheme for `permissions`, for a room whose
/// version has the authorization `rules` (for example `RoomVersionRules::V12.authorization`).
///
/// The `users` map is empty; members with an explicit role are added to it with their
/// role's power level (room creators must not be added in room versions where they have
/// unlimited power). The scheme is:
///
/// - `users_default`: the default role's level (0 or 50);
/// - `events_default` 50, so readers cannot send chat or other timeline events;
/// - `state_default`, `ban`, `kick`, `redact`: 100; `invite`: 50 if
///   [`editors_can_invite`](CalendarPermissions::editors_can_invite), else 100;
/// - `events`: `calendar_event` and `calendar_invite` 50, `calendar_rsvp` 0 if
///   [`readers_can_rsvp`](CalendarPermissions::readers_can_rsvp) else 50 (stable and unstable
///   type each), `m.reaction` and `m.room.redaction` 0, `calendar_info`,
///   `availability_policy` and `availability_window` (both forms) 100, and `m.room.name`,
///   `topic`, `avatar`, `power_levels`, `history_visibility`, `canonical_alias`,
///   `encryption`, `server_acl` 100 and `m.room.tombstone` 100 (150 in room versions with
///   privileged creators, as in Matrix's defaults).
///
/// Fails with [`Error::InvalidPermissions`] if the default role is `Owner`.
pub fn power_levels_for(
    permissions: &CalendarPermissions,
    rules: &AuthorizationRules,
) -> Result<RoomPowerLevelsEventContent> {
    permissions.validate()?;
    let mut content = scheme_json(permissions, rules.explicitly_privilege_room_creators);
    content.insert("users".to_owned(), Value::Object(Map::new()));
    Ok(serde_json::from_value(Value::Object(content))?)
}

// ---------------------------------------------------------------------------------------------
// Classification
// ---------------------------------------------------------------------------------------------

/// The role a user with power level `level` has under `levels`, by capability: Owner if the
/// level allows sending `m.room.power_levels`, else Editor if it allows sending calendar
/// events, else Reader.
pub fn role_for_level(levels: &RoomPowerLevels, level: UserPowerLevel) -> CalendarRole {
    if level >= levels.for_state(StateEventType::RoomPowerLevels) {
        CalendarRole::Owner
    } else if level >= levels.for_message(calendar_event_type()) {
        CalendarRole::Editor
    } else {
        CalendarRole::Reader
    }
}

/// The role of `user_id` under the power levels `levels` (see [`role_for_level`]). Room
/// creators with unlimited power (room version 12+) are always Owners.
pub fn role_from_power_levels(levels: &RoomPowerLevels, user_id: &UserId) -> CalendarRole {
    role_for_level(levels, levels.for_user(user_id))
}

/// Reads the [`CalendarPermissionsSummary`] from `levels`.
pub fn summarize_power_levels(levels: &RoomPowerLevels) -> CalendarPermissionsSummary {
    let default_role = role_for_level(levels, levels.users_default.into());
    let readers_can_rsvp = levels
        .for_message(MessageLikeEventType::from(ids::CALENDAR_RSVP_UNSTABLE))
        <= int(CalendarRole::Reader.power_level());
    let editors_can_invite = levels.invite <= int(CalendarRole::Editor.power_level());
    let mut summary = CalendarPermissionsSummary {
        default_role,
        readers_can_rsvp,
        editors_can_invite,
        is_standard: false,
    };
    summary.is_standard = is_standard(levels, &summary);
    summary
}

fn is_standard(levels: &RoomPowerLevels, summary: &CalendarPermissionsSummary) -> bool {
    if summary.default_role.can_manage() {
        return false;
    }
    let perms = summary.as_permissions();
    let privileged = levels.rules.privileged_creators.is_some();
    let actual_top = |key: &str| -> Int {
        match key {
            "users_default" => levels.users_default,
            "events_default" => levels.events_default,
            "state_default" => levels.state_default,
            "ban" => levels.ban,
            "kick" => levels.kick,
            "redact" => levels.redact,
            _ => levels.invite,
        }
    };
    let top_ok = scheme_top_level(&perms)
        .iter()
        .all(|(key, level)| i64::from(actual_top(key)) == *level);
    let events_ok = scheme_events(&perms, privileged)
        .iter()
        .all(|(event_type, level)| {
            levels
                .events
                .get(&(*event_type).into())
                .is_some_and(|actual| i64::from(*actual) == *level)
        });
    let users_ok = levels.users.values().all(|level| {
        CalendarRole::ALL
            .iter()
            .any(|role| role.power_level() == i64::from(*level))
    });
    top_ok && events_ok && users_ok
}

fn calendar_event_type() -> MessageLikeEventType {
    MessageLikeEventType::from(ids::CALENDAR_EVENT_UNSTABLE)
}

fn describe_level(level: UserPowerLevel) -> String {
    match level {
        UserPowerLevel::Int(level) => level.to_string(),
        _ => "unlimited (room creator)".to_owned(),
    }
}

fn int(level: i64) -> Int {
    Int::new(level).unwrap_or_default()
}

// ---------------------------------------------------------------------------------------------
// Reading power levels
// ---------------------------------------------------------------------------------------------

/// The power levels from the local state store, if they and the room's create event are
/// known.
async fn cached_levels(room: &Room) -> Option<RoomPowerLevels> {
    match room.power_levels().await {
        Ok(levels) => Some(levels),
        Err(error) => {
            debug!(room_id = %room.room_id(), %error, "power levels not available locally");
            None
        }
    }
}

/// The power levels from the local state store, or from the homeserver if they have not
/// been synced yet (for example right after creating the room).
pub(crate) async fn current_levels(room: &Room) -> Result<RoomPowerLevels> {
    match cached_levels(room).await {
        Some(levels) => Ok(levels),
        None => Ok(Snapshot::fetch(room).await?.levels),
    }
}

/// The power levels known **locally**, including the stripped state of a room the current user
/// has only been invited to, where [`Room::power_levels`] finds nothing.
///
/// Never contacts the homeserver: an invited user may not read the room state, and the
/// stripped state is all there is. Returns `None` if neither the synced nor the stripped state
/// has `m.room.power_levels`, or if the room's create event is unknown (so the authorization
/// rules and creators cannot be determined).
pub(crate) async fn local_levels(room: &Room) -> Option<RoomPowerLevels> {
    if let Some(levels) = cached_levels(room).await {
        return Some(levels);
    }
    let raw = match room
        .get_state_event(StateEventType::RoomPowerLevels, "")
        .await
    {
        Ok(raw) => raw?,
        Err(error) => {
            debug!(room_id = %room.room_id(), %error, "could not read the power levels state event");
            return None;
        }
    };
    let json = match &raw {
        RawAnySyncOrStrippedState::Sync(raw) => raw.json(),
        RawAnySyncOrStrippedState::Stripped(raw) => raw.json(),
    };

    #[derive(Deserialize)]
    struct StateEnvelope {
        content: Map<String, Value>,
    }

    let envelope: StateEnvelope = serde_json::from_str(json.get())
        .inspect_err(|error| warn!(%error, "ignoring unparsable power levels state event"))
        .ok()?;
    let creators = room.creators()?;
    let rules = room.clone_info().room_version_rules_or_default();
    levels_from_json(&envelope.content, &rules.authorization, &creators)
        .inspect_err(|error| warn!(%error, "ignoring unparsable power levels content"))
        .ok()
}

/// The role of `user_id` according to the locally known power levels (see [`local_levels`]),
/// or `None` if they are not known locally.
pub(crate) async fn local_role(room: &Room, user_id: &UserId) -> Option<CalendarRole> {
    Some(role_from_power_levels(&local_levels(room).await?, user_id))
}

/// The power levels as currently stored by the homeserver, for read-modify-write updates.
struct Snapshot {
    /// The raw content, so that keys ruma does not know survive a rewrite.
    content: Map<String, Value>,
    levels: RoomPowerLevels,
    rules: AuthorizationRules,
    creators: Vec<OwnedUserId>,
    own: OwnedUserId,
}

impl Snapshot {
    async fn fetch(room: &Room) -> Result<Self> {
        let own = room.own_user_id().to_owned();
        let (rules, creators) = room_rules_and_creators(room).await?;
        let request = get_state_event_for_key::v3::Request::new(
            room.room_id().to_owned(),
            StateEventType::RoomPowerLevels,
            String::new(),
        );
        let content = match room.client().send(request).await {
            Ok(response) => {
                match serde_json::from_str::<Value>(response.into_content().json().get())? {
                    Value::Object(map) => map,
                    _ => return Err(Error::Other("power levels content is not an object".into())),
                }
            }
            Err(error) if is_not_found(&error) => {
                // No power levels event: the spec's defaults apply.
                let defaults = RoomPowerLevels::new(
                    RoomPowerLevelsSource::None,
                    &rules,
                    creators.iter().cloned(),
                );
                let content = RoomPowerLevelsEventContent::try_from(defaults)
                    .map_err(|e| Error::Other(e.to_string()))?;
                match serde_json::to_value(content)? {
                    Value::Object(map) => map,
                    _ => Map::new(),
                }
            }
            Err(error) => return Err(error.into()),
        };
        let levels = levels_from_json(&content, &rules, &creators)?;
        Ok(Self {
            content,
            levels,
            rules,
            creators,
            own,
        })
    }

    fn privileged(&self) -> bool {
        self.rules.explicitly_privilege_room_creators
    }

    fn is_privileged_creator(&self, user_id: &UserId) -> bool {
        self.privileged() && self.creators.iter().any(|c| c == user_id)
    }

    /// The explicit level of `user_id` in the `users` map.
    fn explicit_level(&self, user_id: &UserId) -> Option<i64> {
        self.levels
            .users
            .get(user_id)
            .map(|level| i64::from(*level))
    }

    fn own_level(&self) -> UserPowerLevel {
        self.levels.for_user(&self.own)
    }

    fn own_role(&self) -> CalendarRole {
        role_from_power_levels(&self.levels, &self.own)
    }

    /// The content with `user_id` set to `level` (or removed with `None`), and the resulting
    /// levels.
    fn with_user_level(
        &self,
        user_id: &UserId,
        level: Option<i64>,
    ) -> Result<(Map<String, Value>, RoomPowerLevels)> {
        let mut content = self.content.clone();
        let users = content
            .entry("users")
            .or_insert_with(|| Value::Object(Map::new()));
        if !users.is_object() {
            *users = Value::Object(Map::new());
        }
        if let Value::Object(users) = users {
            match level {
                Some(level) => {
                    users.insert(user_id.to_string(), json!(level));
                }
                None => {
                    users.remove(user_id.as_str());
                }
            }
        }
        let levels = levels_from_json(&content, &self.rules, &self.creators)?;
        Ok((content, levels))
    }

    /// Checks that the own user may set the level of `user_id` to `role`'s level.
    fn check_can_set(&self, user_id: &UserId, role: CalendarRole) -> Result<()> {
        let own = &self.own;
        if !self
            .levels
            .user_can_send_state(own, StateEventType::RoomPowerLevels)
        {
            return Err(Error::PermissionDenied(format!(
                "{own} cannot change roles in this calendar: that requires the owner role \
                 (power level {} to send m.room.power_levels), but {own} has the {} role with \
                 power level {}",
                self.levels.for_state(StateEventType::RoomPowerLevels),
                self.own_role(),
                describe_level(self.own_level()),
            )));
        }
        if !self.levels.user_can_change_user_power_level(own, user_id) {
            return Err(Error::PermissionDenied(format!(
                "{own} cannot change the role of {user_id}: their power level {} is not lower \
                 than {own}'s ({})",
                describe_level(self.levels.for_user(user_id)),
                describe_level(self.own_level()),
            )));
        }
        if self.own_level() < int(role.power_level()) {
            return Err(Error::PermissionDenied(format!(
                "{own} cannot grant the {role} role (power level {}): it is higher than \
                 {own}'s own power level {}",
                role.power_level(),
                describe_level(self.own_level()),
            )));
        }
        Ok(())
    }

    /// Refuses to demote the own user if no other owner is joined.
    async fn check_not_last_owner(&self, room: &Room) -> Result<()> {
        if role_for_level(&self.levels, self.levels.users_default.into()).can_manage() {
            // Everyone is an owner.
            return Ok(());
        }
        let mut candidates: Vec<OwnedUserId> = self
            .levels
            .users
            .keys()
            .filter(|user| **user != self.own)
            .filter(|user| role_from_power_levels(&self.levels, user).can_manage())
            .cloned()
            .collect();
        if self.privileged() {
            candidates.extend(self.creators.iter().filter(|c| **c != self.own).cloned());
        }
        for candidate in &candidates {
            if fetch_membership(room, candidate).await? == Some(MembershipState::Join) {
                return Ok(());
            }
        }
        Err(Error::PermissionDenied(format!(
            "{} is the last owner of this calendar{}; make another member an owner before \
             giving up the owner role",
            self.own,
            if candidates.is_empty() {
                String::new()
            } else {
                format!(" (other owners {candidates:?} have not joined)")
            }
        )))
    }

    async fn send(&self, room: &Room, content: Map<String, Value>) -> Result<()> {
        room.send_state_event_raw(POWER_LEVELS, "", Value::Object(content))
            .await?;
        Ok(())
    }
}

fn levels_from_json(
    content: &Map<String, Value>,
    rules: &AuthorizationRules,
    creators: &[OwnedUserId],
) -> Result<RoomPowerLevels> {
    let parsed: RoomPowerLevelsEventContent =
        serde_json::from_value(Value::Object(content.clone()))?;
    Ok(RoomPowerLevels::new(
        parsed.into(),
        rules,
        creators.iter().cloned(),
    ))
}

/// The authorization rules and creators of `room`, from the local state if its create event
/// has been synced, otherwise from the homeserver.
async fn room_rules_and_creators(room: &Room) -> Result<(AuthorizationRules, Vec<OwnedUserId>)> {
    if let Some(creators) = room.creators() {
        let rules = room.clone_info().room_version_rules_or_default();
        return Ok((rules.authorization, creators));
    }

    #[derive(Deserialize)]
    struct CreateEvent {
        sender: OwnedUserId,
        #[serde(default)]
        content: CreateContent,
    }
    #[derive(Default, Deserialize)]
    struct CreateContent {
        room_version: Option<String>,
        creator: Option<OwnedUserId>,
        #[serde(default)]
        additional_creators: Vec<OwnedUserId>,
    }

    let state = room
        .client()
        .send(get_state_events::v3::Request::new(
            room.room_id().to_owned(),
        ))
        .await?
        .room_state;
    let create = state
        .iter()
        .find(|ev| {
            ev.get_field::<String>("type").ok().flatten().as_deref() == Some("m.room.create")
        })
        .ok_or_else(|| Error::Other(format!("{} has no m.room.create event", room.room_id())))?;
    let create: CreateEvent = serde_json::from_str(create.json().get())?;
    let rules = create
        .content
        .room_version
        .as_deref()
        .map_or(Some(RoomVersionId::V1), |v| RoomVersionId::try_from(v).ok())
        .and_then(|v| v.rules())
        .unwrap_or(RoomVersionRules::V11);
    let creators = if rules.authorization.explicitly_privilege_room_creators {
        std::iter::once(create.sender)
            .chain(create.content.additional_creators)
            .collect()
    } else {
        vec![create.content.creator.unwrap_or(create.sender)]
    };
    Ok((rules.authorization, creators))
}

/// The membership of `user_id` in `room` according to the homeserver, or `None` if the user
/// has never been a member.
async fn fetch_membership(room: &Room, user_id: &UserId) -> Result<Option<MembershipState>> {
    let request = get_state_event_for_key::v3::Request::new(
        room.room_id().to_owned(),
        StateEventType::RoomMember,
        user_id.to_string(),
    );
    match room.client().send(request).await {
        Ok(response) => {
            let membership = response
                .into_content()
                .get_field::<MembershipState>("membership")?;
            Ok(membership)
        }
        Err(error) if is_not_found(&error) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn is_not_found(error: &HttpError) -> bool {
    error
        .as_client_api_error()
        .is_some_and(|e| e.status_code.as_u16() == 404)
}

// ---------------------------------------------------------------------------------------------
// Client-side guards
// ---------------------------------------------------------------------------------------------

/// Fails with [`Error::PermissionDenied`] if the locally known power levels do not let the
/// own user send message-like events of `event_type`. Does nothing if the power levels have
/// not been synced; the homeserver remains the authority.
pub(crate) async fn ensure_can_send_message(
    room: &Room,
    event_type: &str,
    action: &str,
) -> Result<()> {
    let Some(levels) = cached_levels(room).await else {
        return Ok(());
    };
    let own = room.own_user_id();
    let required = levels.for_message(MessageLikeEventType::from(event_type));
    let level = levels.for_user(own);
    if level >= required {
        return Ok(());
    }
    Err(Error::PermissionDenied(format!(
        "{own} cannot {action}: sending {event_type} requires power level {required}, but \
         {own} has power level {} (role: {}); ask an owner of the calendar for the editor role",
        describe_level(level),
        role_for_level(&levels, level),
    )))
}

/// Like [`ensure_can_send_message`] for a state event type.
pub(crate) async fn ensure_can_send_state(
    room: &Room,
    event_type: &str,
    action: &str,
) -> Result<()> {
    let Some(levels) = cached_levels(room).await else {
        return Ok(());
    };
    let own = room.own_user_id();
    let required = levels.for_state(StateEventType::from(event_type));
    let level = levels.for_user(own);
    if level >= required {
        return Ok(());
    }
    Err(Error::PermissionDenied(format!(
        "{own} cannot {action}: sending the {event_type} state event requires power level \
         {required}, but {own} has power level {} (role: {})",
        describe_level(level),
        role_for_level(&levels, level),
    )))
}

/// The guard for every operation that sends calendar events.
pub(crate) async fn ensure_can_write(room: &Room, action: &str) -> Result<()> {
    ensure_can_send_message(room, ids::CALENDAR_EVENT_UNSTABLE, action).await
}

// ---------------------------------------------------------------------------------------------
// Room operations (called through `Calendar`)
// ---------------------------------------------------------------------------------------------

pub(crate) async fn role_of(room: &Room, user_id: &UserId) -> Result<CalendarRole> {
    Ok(role_from_power_levels(
        &current_levels(room).await?,
        user_id,
    ))
}

pub(crate) async fn permissions(room: &Room) -> Result<CalendarPermissionsSummary> {
    Ok(summarize_power_levels(&current_levels(room).await?))
}

pub(crate) async fn members_with_roles(room: &Room) -> Result<Vec<CalendarMember>> {
    let levels = current_levels(room).await?;
    let members = room
        .members(RoomMemberships::JOIN | RoomMemberships::INVITE)
        .await?;
    let mut out: Vec<CalendarMember> = members
        .iter()
        .filter_map(|member| {
            let membership = match member.membership() {
                MembershipState::Join => CalendarMembership::Joined,
                MembershipState::Invite => CalendarMembership::Invited,
                _ => return None,
            };
            Some(CalendarMember {
                user_id: member.user_id().to_owned(),
                display_name: member.display_name().map(ToOwned::to_owned),
                membership,
                role: role_from_power_levels(&levels, member.user_id()),
            })
        })
        .collect();
    out.sort_by(|a, b| (Reverse(a.role), &a.user_id).cmp(&(Reverse(b.role), &b.user_id)));
    Ok(out)
}

pub(crate) async fn invite(room: &Room, user_id: &UserId, role: CalendarRole) -> Result<()> {
    if user_id == room.own_user_id() {
        return Err(Error::Other(
            "you are already a member of this calendar; use set_role to change your own role"
                .into(),
        ));
    }
    let snapshot = Snapshot::fetch(room).await?;
    let own = snapshot.own.clone();
    let membership = fetch_membership(room, user_id).await?;
    if membership == Some(MembershipState::Ban) {
        return Err(Error::PermissionDenied(format!(
            "{user_id} is banned from this calendar and cannot be invited"
        )));
    }
    let already_member = matches!(
        membership,
        Some(MembershipState::Join | MembershipState::Invite)
    );

    // Decide whether the power levels need to change.
    let new_content = if snapshot.is_privileged_creator(user_id) {
        if role != CalendarRole::Owner {
            return Err(creator_error(user_id, role));
        }
        None
    } else if snapshot.explicit_level(user_id) == Some(role.power_level()) {
        None
    } else {
        match snapshot.check_can_set(user_id, role) {
            Ok(()) => {
                let (content, levels) =
                    snapshot.with_user_level(user_id, Some(role.power_level()))?;
                check_resulting_role(&levels, user_id, role)?;
                Some(content)
            }
            // Members who may invite but not change roles (editors with
            // `editors_can_invite`) can still invite users with the default role.
            Err(_)
                if !already_member
                    && snapshot.explicit_level(user_id).is_none()
                    && role_from_power_levels(&snapshot.levels, user_id) == role =>
            {
                None
            }
            Err(error) => return Err(error),
        }
    };

    if !already_member && !snapshot.levels.user_can_invite(&own) {
        return Err(Error::PermissionDenied(format!(
            "{own} cannot invite users to this calendar: inviting requires power level {}, but \
             {own} has power level {} (role: {})",
            snapshot.levels.invite,
            describe_level(snapshot.own_level()),
            snapshot.own_role(),
        )));
    }

    // The role first, so that the user never holds the invite with a wrong role.
    if let Some(content) = new_content {
        snapshot.send(room, content).await?;
    }
    if !already_member {
        room.invite_user_by_id(user_id).await?;
    }
    Ok(())
}

pub(crate) async fn set_role(room: &Room, user_id: &UserId, role: CalendarRole) -> Result<()> {
    let snapshot = Snapshot::fetch(room).await?;
    if snapshot.is_privileged_creator(user_id) {
        if role == CalendarRole::Owner {
            return Ok(());
        }
        return Err(creator_error(user_id, role));
    }
    snapshot.check_can_set(user_id, role)?;
    if user_id == snapshot.own && role < CalendarRole::Owner {
        snapshot.check_not_last_owner(room).await?;
    }
    if snapshot.explicit_level(user_id) == Some(role.power_level()) {
        return Ok(());
    }
    let (content, levels) = snapshot.with_user_level(user_id, Some(role.power_level()))?;
    check_resulting_role(&levels, user_id, role)?;
    snapshot.send(room, content).await
}

pub(crate) async fn remove_member(
    room: &Room,
    user_id: &UserId,
    reason: Option<&str>,
) -> Result<()> {
    let own = room.own_user_id();
    if user_id == own {
        return Err(Error::Other(
            "remove_member cannot remove yourself; leave the room instead".into(),
        ));
    }
    if let Some(levels) = cached_levels(room).await
        && !levels.user_can_kick_user(own, user_id)
    {
        return Err(Error::PermissionDenied(format!(
            "{own} cannot remove {user_id}: removing members requires power level {} and a \
             higher power level than the member's ({}), but {own} has power level {}",
            levels.kick,
            describe_level(levels.for_user(user_id)),
            describe_level(levels.for_user(own)),
        )));
    }
    room.kick_user(user_id, reason).await?;

    // Drop their explicit role, so that a later plain invite does not restore it.
    let cleanup = async {
        let snapshot = Snapshot::fetch(room).await?;
        if snapshot.explicit_level(user_id).is_some()
            && snapshot
                .levels
                .user_can_change_user_power_level(&snapshot.own, user_id)
        {
            let (content, _) = snapshot.with_user_level(user_id, None)?;
            snapshot.send(room, content).await?;
        }
        Ok::<_, Error>(())
    };
    if let Err(error) = cleanup.await {
        warn!(%user_id, %error, "removed member, but could not clear their role");
    }
    Ok(())
}

pub(crate) async fn apply_permissions(
    room: &Room,
    permissions: &CalendarPermissions,
    normalize_users: bool,
) -> Result<()> {
    permissions.validate()?;
    let snapshot = Snapshot::fetch(room).await?;
    let own = snapshot.own.clone();
    if !snapshot.own_role().can_manage() {
        return Err(Error::PermissionDenied(format!(
            "{own} cannot change the permissions of this calendar: that requires the owner \
             role, but {own} has the {} role",
            snapshot.own_role()
        )));
    }

    let mut content = snapshot.content.clone();
    apply_scheme(&mut content, permissions, snapshot.privileged());
    if normalize_users && let Some(Value::Object(users)) = content.get_mut("users") {
        for (user, value) in users.iter_mut() {
            let Some(level) = value.as_i64() else {
                continue;
            };
            let nearest = CalendarRole::nearest(level).power_level();
            if nearest == level {
                continue;
            }
            let Ok(user_id) = UserId::parse(user.as_str()) else {
                continue;
            };
            // Users at or above the own level cannot be changed (except oneself).
            if user_id != own && snapshot.levels.for_user(&user_id) >= snapshot.own_level() {
                warn!(%user_id, level, "not normalizing a user with a power level not below ours");
                continue;
            }
            *value = json!(nearest);
        }
    }
    let levels = levels_from_json(&content, &snapshot.rules, &snapshot.creators)?;
    if !role_from_power_levels(&levels, &own).can_manage() {
        return Err(Error::PermissionDenied(format!(
            "applying these permissions would remove {own}'s owner role (power level {} is \
             below the scheme's owner level {}); raise it first",
            describe_level(snapshot.own_level()),
            CalendarRole::Owner.power_level()
        )));
    }
    if content == snapshot.content {
        return Ok(());
    }
    snapshot.send(room, content).await
}

pub(crate) async fn set_default_role(room: &Room, role: CalendarRole) -> Result<()> {
    let snapshot = Snapshot::fetch(room).await?;
    let mut permissions = summarize_power_levels(&snapshot.levels).as_permissions();
    permissions.default_role = role;
    apply_permissions(room, &permissions, false).await
}

fn creator_error(user_id: &UserId, role: CalendarRole) -> Error {
    Error::PermissionDenied(format!(
        "{user_id} created this calendar room; in room version 12 and later room creators have \
         unlimited power, are always owners and cannot be given the {role} role"
    ))
}

/// Checks that `user_id` has `role` under the new `levels`; fails for rooms whose power
/// levels do not follow the scheme (for example a reader level that can still send calendar
/// events).
fn check_resulting_role(
    levels: &RoomPowerLevels,
    user_id: &UserId,
    role: CalendarRole,
) -> Result<()> {
    let actual = role_from_power_levels(levels, user_id);
    if actual == role {
        return Ok(());
    }
    Err(Error::InvalidPermissions(format!(
        "giving {user_id} power level {} would make them a {actual}, not a {role}, because the \
         calendar's power levels do not follow the SDK's scheme; call apply_permissions first",
        role.power_level()
    )))
}

// ---------------------------------------------------------------------------------------------
// Room creation
// ---------------------------------------------------------------------------------------------

/// The `power_level_content_override` for a new calendar room, and the room version to pin
/// in the request (if known).
///
/// Synapse merges the override's top-level keys into its defaults, so every key of the
/// scheme is spelled out. The `users` map is only sent when someone gets an explicit role,
/// and then needs to know whether the creator belongs into it (room versions before 12) or
/// must not appear in it (12 and later): the version is `room_version` if given, else the
/// server's default room version from the capabilities.
pub(crate) async fn creation_override(
    client: &Client,
    permissions: &CalendarPermissions,
    explicit_roles: &BTreeMap<OwnedUserId, CalendarRole>,
    room_version: Option<RoomVersionId>,
) -> Result<(Raw<RoomPowerLevelsContentOverride>, Option<RoomVersionId>)> {
    permissions.validate()?;
    let room_version = match room_version {
        Some(version) => Some(version),
        None => match client.homeserver_capabilities().room_versions().await {
            Ok(capability) => Some(capability.default),
            Err(error) => {
                warn!(%error, "could not read the default room version from the capabilities");
                None
            }
        },
    };
    // Versions ruma does not know are newer than 12, so they keep privileged creators. If
    // the version is unknown altogether, assume an older server: putting the creator into
    // `users` then fails loudly on a server that defaults to version 12 (the server rejects
    // the request) instead of silently locking the creator out on an older one.
    let privileged = match &room_version {
        Some(version) => version
            .rules()
            .is_none_or(|rules| rules.authorization.explicitly_privilege_room_creators),
        None => false,
    };

    let mut content = scheme_json(permissions, privileged);
    if !explicit_roles.is_empty() {
        let mut users = Map::new();
        if !privileged && let Some(own) = client.user_id() {
            users.insert(own.to_string(), json!(CalendarRole::Owner.power_level()));
        }
        for (user, role) in explicit_roles {
            users.insert(user.to_string(), json!(role.power_level()));
        }
        content.insert("users".to_owned(), Value::Object(users));
    }
    let raw = Raw::from_json(serde_json::value::to_raw_value(&Value::Object(content))?);
    Ok((raw, room_version))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearest_role() {
        assert_eq!(CalendarRole::nearest(-5), CalendarRole::Reader);
        assert_eq!(CalendarRole::nearest(25), CalendarRole::Reader);
        assert_eq!(CalendarRole::nearest(26), CalendarRole::Editor);
        assert_eq!(CalendarRole::nearest(75), CalendarRole::Editor);
        assert_eq!(CalendarRole::nearest(76), CalendarRole::Owner);
        assert_eq!(CalendarRole::nearest(9000), CalendarRole::Owner);
    }

    #[test]
    fn apply_scheme_keeps_unknown_keys() {
        let mut content = serde_json::from_value::<Map<String, Value>>(json!({
            "historical": 100,
            "users": { "@a:x": 75 },
            "events": { "m.room.pinned_events": 50, "m.room.name": 50 },
        }))
        .unwrap();
        apply_scheme(&mut content, &CalendarPermissions::default(), false);
        assert_eq!(content["historical"], 100);
        assert_eq!(content["users"], json!({ "@a:x": 75 }));
        assert_eq!(content["events"]["m.room.pinned_events"], 50);
        assert_eq!(content["events"]["m.room.name"], 100);
        assert_eq!(content["events_default"], 50);
    }
}
