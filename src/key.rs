//! The identity of a calendar event across the SDK.

use matrix_sdk::ruma::OwnedUserId;
use serde::{Deserialize, Serialize};

/// The identity of a calendar event: UIDs are only unique per organiser (MSC4496, "Security
/// considerations"), so every index in this crate keys events by `(organiser, uid)`.
///
/// - In a [`CalendarIndex`](crate::calendar::CalendarIndex), `sender` is the sender of the
///   original `calendar_event` (only the sender can edit it, so it is the organiser).
/// - In a [`SchedulingIndex`](crate::scheduling::SchedulingIndex), `sender` is the organiser
///   of the invite: the `calendar_event.organizer` it names, or its sender if it names none.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct EventKey {
    /// The organiser (see the type documentation).
    pub sender: OwnedUserId,
    /// The event's `uid`.
    pub uid: String,
}

impl EventKey {
    /// Creates a key.
    pub fn new(sender: OwnedUserId, uid: impl Into<String>) -> Self {
        Self {
            sender,
            uid: uid.into(),
        }
    }
}
