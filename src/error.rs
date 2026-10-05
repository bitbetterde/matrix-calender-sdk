//! Error type used throughout the crate.

use matrix_sdk::ruma::OwnedRoomId;

/// Errors produced by this crate.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A timezone identifier is not a known IANA timezone.
    #[error("invalid timezone: {0}")]
    InvalidTimezone(String),

    /// A date or time value is malformed or does not exist (for example a local time that
    /// falls into a DST gap).
    #[error("invalid date/time: {0}")]
    InvalidDateTime(String),

    /// The `utc` and `localtime` fields of a timed [`EventTime`](crate::events::EventTime) do not
    /// describe the same instant in the given timezone, or `start`/`end` are inconsistent.
    #[error("inconsistent date/time: {0}")]
    InconsistentDateTime(String),

    /// An identifier (room type, event type, uid, rel_type, ...) is invalid or unexpected.
    #[error("invalid identifier: {0}")]
    InvalidIdentifier(String),

    /// A colour value is not of the form `#rrggbb`.
    #[error("invalid colour (expected #rrggbb): {0}")]
    InvalidColor(String),

    /// A recurrence rule is malformed.
    #[error("invalid recurrence rule: {0}")]
    InvalidRecurrence(String),

    /// A duration is not a valid ISO 8601 signed duration (see
    /// [`SignedDuration`](crate::events::SignedDuration)) or does not fit into the supported
    /// range.
    #[error("invalid duration: {0}")]
    InvalidDuration(String),

    /// JSON (de)serialization failed.
    #[error(transparent)]
    Serde(#[from] serde_json::Error),

    /// An error returned by `matrix-sdk`.
    #[error(transparent)]
    Sdk(Box<matrix_sdk::Error>),

    /// An HTTP error returned by `matrix-sdk`.
    #[error(transparent)]
    Http(Box<matrix_sdk::HttpError>),

    /// The room is not a calendar room (its room type is neither the stable nor the unstable
    /// calendar room type).
    #[error("room {0} is not a calendar room")]
    NotACalendarRoom(OwnedRoomId),

    /// The operation is not permitted, for example editing an event that was sent by
    /// another user, or sending a calendar event with the reader role. The message names the
    /// missing permission.
    #[error("permission denied: {0}")]
    PermissionDenied(String),

    /// Calendar permissions are invalid (for example the owner role as the default role), or
    /// cannot be applied to a room whose power levels do not follow the SDK's scheme.
    #[error("invalid permissions: {0}")]
    InvalidPermissions(String),

    /// Any other error.
    #[error("{0}")]
    Other(String),
}

impl From<matrix_sdk::Error> for Error {
    fn from(err: matrix_sdk::Error) -> Self {
        Self::Sdk(Box::new(err))
    }
}

impl From<matrix_sdk::HttpError> for Error {
    fn from(err: matrix_sdk::HttpError) -> Self {
        Self::Http(Box::new(err))
    }
}

/// Result type alias using the crate's [`Error`].
pub type Result<T> = std::result::Result<T, Error>;
