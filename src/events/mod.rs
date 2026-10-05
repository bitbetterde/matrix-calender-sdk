//! Event content types for MSC4496, with serde matching the MSC's JSON wire format.
//!
//! All event types are sent with their unstable identifiers
//! (`org.matrix.msc4496.*`) and accept the stable ones (`m.calendar.*`) on input.

/// Defines a `#[non_exhaustive]` enum of known wire strings plus a catch-all `Other(String)`
/// variant, so that unknown values from newer clients or bridges deserialize and round-trip
/// instead of failing.
///
/// Generates `as_str`, `Display`, `From<&str>`, `Serialize` and `Deserialize`. Known strings
/// are matched exactly (case-sensitively); `Other` must not be constructed with a known string.
macro_rules! string_enum {
    (
        $(#[$meta:meta])*
        $vis:vis enum $name:ident {
            $(
                $(#[$vmeta:meta])*
                $variant:ident = $wire:literal,
            )+
        }
    ) => {
        $(#[$meta])*
        #[non_exhaustive]
        #[derive(Clone, Debug, PartialEq, Eq, Hash)]
        $vis enum $name {
            $(
                $(#[$vmeta])*
                $variant,
            )+
            /// Any other value, kept verbatim.
            Other(String),
        }

        impl $name {
            /// The wire string of this value.
            pub fn as_str(&self) -> &str {
                match self {
                    $( Self::$variant => $wire, )+
                    Self::Other(s) => s,
                }
            }
        }

        impl From<&str> for $name {
            fn from(s: &str) -> Self {
                match s {
                    $( $wire => Self::$variant, )+
                    _ => Self::Other(s.to_owned()),
                }
            }
        }

        impl ::std::fmt::Display for $name {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl ::serde::Serialize for $name {
            fn serialize<S: ::serde::Serializer>(
                &self,
                serializer: S,
            ) -> ::std::result::Result<S::Ok, S::Error> {
                serializer.serialize_str(self.as_str())
            }
        }

        impl<'de> ::serde::Deserialize<'de> for $name {
            fn deserialize<D: ::serde::Deserializer<'de>>(
                deserializer: D,
            ) -> ::std::result::Result<Self, D::Error> {
                let s = <::std::borrow::Cow<'de, str> as ::serde::Deserialize>::deserialize(
                    deserializer,
                )?;
                Ok(Self::from(s.as_ref()))
            }
        }
    };
}

pub mod alert;
pub mod availability;
pub mod calendar_event;
pub mod calendar_info;
pub mod conference;
pub mod datetime;
pub mod duration;
pub mod invite;
pub mod location;
pub mod recurrence;
pub mod rsvp;
pub mod text;

pub use alert::{ALERT_TYPE, Alert, AlertAction, AlertRelativeTo, AlertTrigger};
pub use availability::{
    AvailabilityPolicyEventContent, AvailabilityRequest, AvailabilityResponse,
    AvailabilityWindowEventContent, BusyInterval, BusyType, Grant, TIER_BUSY, TIER_NONE,
    TIER_TITLES, Window,
};
pub use calendar_event::{
    CalendarEvent, CalendarEventEventContent, CalendarEventEventContentWithoutRelation,
    CalendarEventRelation, CalendarOverride, Status, Transparency,
};
pub use calendar_info::{CalendarInfo, CalendarInfoEventContent, is_valid_color};
pub use conference::Conference;
pub use datetime::EventTime;
pub use duration::SignedDuration;
pub use invite::{CalendarInvite, CalendarInviteEventContent, InviteEventSummary, InviteMethod};
pub use location::{Location, LocationType};
pub use recurrence::{ByDay, Frequency, RRule, Recurrence, Until, Weekday};
pub use rsvp::{CalendarRsvp, CalendarRsvpEventContent, PartStat};
