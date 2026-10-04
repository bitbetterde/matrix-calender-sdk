//! Identifiers defined by MSC4496, in their stable and unstable forms.
//!
//! The SDK **sends** the unstable identifiers (as required while the MSC is not accepted) and
//! **accepts** both forms when receiving. Use the `is_*` helpers to check an identifier received
//! from the wire.

/// Stable calendar room type (`m.room.create` content `type`).
pub const ROOM_TYPE_CALENDAR: &str = "m.calendar";
/// Unstable calendar room type.
pub const ROOM_TYPE_CALENDAR_UNSTABLE: &str = "org.matrix.msc4496.calendar";

/// Stable `calendar_info` state event type.
pub const CALENDAR_INFO: &str = "m.calendar.info";
/// Unstable `calendar_info` state event type (also the content block name).
pub const CALENDAR_INFO_UNSTABLE: &str = "org.matrix.msc4496.calendar_info";

/// Stable calendar event type and content block name.
pub const CALENDAR_EVENT: &str = "m.calendar.event";
/// Unstable calendar event type and content block name.
pub const CALENDAR_EVENT_UNSTABLE: &str = "org.matrix.msc4496.calendar_event";

/// Stable calendar invite event type and content block name.
pub const CALENDAR_INVITE: &str = "m.calendar.invite";
/// Unstable calendar invite event type and content block name.
pub const CALENDAR_INVITE_UNSTABLE: &str = "org.matrix.msc4496.calendar_invite";

/// Stable calendar RSVP event type and content block name.
pub const CALENDAR_RSVP: &str = "m.calendar.rsvp";
/// Unstable calendar RSVP event type and content block name.
pub const CALENDAR_RSVP_UNSTABLE: &str = "org.matrix.msc4496.calendar_rsvp";

/// Stable `rel_type` for recurrence instance overrides.
pub const CALENDAR_OVERRIDE_REL_TYPE: &str = "m.calendar.override";
/// Unstable `rel_type` for recurrence instance overrides.
pub const CALENDAR_OVERRIDE_REL_TYPE_UNSTABLE: &str = "org.matrix.msc4496.calendar_override";

/// Stable availability policy state event type.
pub const AVAILABILITY_POLICY: &str = "m.calendar.availability_policy";
/// Unstable availability policy state event type.
pub const AVAILABILITY_POLICY_UNSTABLE: &str = "org.matrix.msc4496.availability_policy";

/// Stable availability (working hours) window state event type.
pub const AVAILABILITY_WINDOW: &str = "m.calendar.availability_window";
/// Unstable availability (working hours) window state event type.
pub const AVAILABILITY_WINDOW_UNSTABLE: &str = "org.matrix.msc4496.availability_window";

/// Stable profile field name for the working-hours window (MSC4133 extensible profiles).
pub const WORKING_HOURS_PROFILE_FIELD: &str = "m.calendar.working_hours";
/// Unstable profile field name for the working-hours window.
pub const WORKING_HOURS_PROFILE_FIELD_UNSTABLE: &str = "org.matrix.msc4496.working_hours";

/// Stable client-server availability endpoint path template (`POST`).
pub const CLIENT_AVAILABILITY_PATH: &str = "/_matrix/client/v1/user/{userId}/availability";
/// Unstable client-server availability endpoint path template (`POST`).
pub const CLIENT_AVAILABILITY_PATH_UNSTABLE: &str =
    "/_matrix/client/unstable/org.matrix.msc4496/user/{userId}/availability";

/// Stable federation availability endpoint path template (`GET`).
pub const FEDERATION_AVAILABILITY_PATH: &str = "/_matrix/federation/v1/user/{userId}/availability";
/// Unstable federation availability endpoint path template (`GET`).
pub const FEDERATION_AVAILABILITY_PATH_UNSTABLE: &str =
    "/_matrix/federation/unstable/org.matrix.msc4496/user/{userId}/availability";

/// The MSC1767 text content block name used by the MSC examples.
pub const TEXT_BLOCK: &str = "m.text";
/// The unstable MSC1767 text content block name (accepted on input).
pub const TEXT_BLOCK_UNSTABLE: &str = "org.matrix.msc1767.text";

/// Whether `s` is the stable or unstable calendar room type.
pub fn is_calendar_room_type(s: &str) -> bool {
    s == ROOM_TYPE_CALENDAR || s == ROOM_TYPE_CALENDAR_UNSTABLE
}

/// Whether `s` is the stable or unstable `calendar_info` event type.
pub fn is_calendar_info_type(s: &str) -> bool {
    s == CALENDAR_INFO || s == CALENDAR_INFO_UNSTABLE
}

/// Whether `s` is the stable or unstable calendar event type.
pub fn is_calendar_event_type(s: &str) -> bool {
    s == CALENDAR_EVENT || s == CALENDAR_EVENT_UNSTABLE
}

/// Whether `s` is the stable or unstable calendar invite event type.
pub fn is_calendar_invite_type(s: &str) -> bool {
    s == CALENDAR_INVITE || s == CALENDAR_INVITE_UNSTABLE
}

/// Whether `s` is the stable or unstable calendar RSVP event type.
pub fn is_calendar_rsvp_type(s: &str) -> bool {
    s == CALENDAR_RSVP || s == CALENDAR_RSVP_UNSTABLE
}

/// Whether `s` is the stable or unstable override `rel_type`.
pub fn is_calendar_override_rel_type(s: &str) -> bool {
    s == CALENDAR_OVERRIDE_REL_TYPE || s == CALENDAR_OVERRIDE_REL_TYPE_UNSTABLE
}

/// Whether `s` is the stable or unstable availability policy event type.
pub fn is_availability_policy_type(s: &str) -> bool {
    s == AVAILABILITY_POLICY || s == AVAILABILITY_POLICY_UNSTABLE
}

/// Whether `s` is the stable or unstable availability window event type.
pub fn is_availability_window_type(s: &str) -> bool {
    s == AVAILABILITY_WINDOW || s == AVAILABILITY_WINDOW_UNSTABLE
}

/// Whether `s` is the stable or unstable working-hours profile field name.
pub fn is_working_hours_profile_field(s: &str) -> bool {
    s == WORKING_HOURS_PROFILE_FIELD || s == WORKING_HOURS_PROFILE_FIELD_UNSTABLE
}

/// Whether `s` is any of the calendar event types defined by MSC4496 (`calendar_info`,
/// `calendar_event`, `calendar_invite`, `calendar_rsvp`, `availability_policy`,
/// `availability_window`), in stable or unstable form.
pub fn is_msc4496_event_type(s: &str) -> bool {
    is_calendar_info_type(s)
        || is_calendar_event_type(s)
        || is_calendar_invite_type(s)
        || is_calendar_rsvp_type(s)
        || is_availability_policy_type(s)
        || is_availability_window_type(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers_accept_both_forms() {
        assert!(is_calendar_room_type("m.calendar"));
        assert!(is_calendar_room_type("org.matrix.msc4496.calendar"));
        assert!(!is_calendar_room_type("m.space"));
        assert!(is_calendar_event_type("m.calendar.event"));
        assert!(is_calendar_event_type("org.matrix.msc4496.calendar_event"));
        assert!(is_calendar_override_rel_type("m.calendar.override"));
        assert!(is_calendar_override_rel_type(
            "org.matrix.msc4496.calendar_override"
        ));
        assert!(is_availability_window_type(
            "m.calendar.availability_window"
        ));
        assert!(is_working_hours_profile_field(
            "org.matrix.msc4496.working_hours"
        ));
        assert!(is_msc4496_event_type("org.matrix.msc4496.calendar_rsvp"));
        assert!(!is_msc4496_event_type("m.room.message"));
    }
}
