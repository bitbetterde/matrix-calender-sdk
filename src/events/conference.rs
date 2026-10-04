//! MatrixRTC video conference attachment (MSC4496 §2.4).

use matrix_sdk::ruma::{OwnedRoomId, OwnedServerName};
use serde::{Deserialize, Serialize};

/// The `type` value of a MatrixRTC conference block.
pub const CONFERENCE_TYPE_RTC: &str = "m.rtc";

/// The MatrixRTC application type for voice/video calls (MSC4196).
pub const CONFERENCE_APPLICATION_CALL: &str = "m.call";

/// A MatrixRTC conference attached to an event.
///
/// Clients must get explicit user confirmation before joining the referenced room, and must
/// not present `label` as the identity of that room (see the MSC's "Call room linkage"
/// security consideration).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Conference {
    /// Conference type; [`CONFERENCE_TYPE_RTC`] (`m.rtc`) for MatrixRTC.
    ///
    /// Kept as a string so that future conference types do not break deserialization.
    #[serde(rename = "type")]
    pub kind: String,
    /// MatrixRTC application type, e.g. [`CONFERENCE_APPLICATION_CALL`].
    pub application: String,
    /// The MatrixRTC-enabled room hosting the call.
    pub room_id: OwnedRoomId,
    /// `state_key` of the `m.rtc.slot` state event; `None` refers to the default slot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot_id: Option<String>,
    /// Servers through which `room_id` can be joined. Missing on input means unresolvable.
    #[serde(default)]
    pub via: Vec<OwnedServerName>,
    /// Free-form action label chosen by the author, e.g. "Join call".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

impl Conference {
    /// A MatrixRTC `m.call` conference in `room_id`, joinable via `via`.
    pub fn call(room_id: OwnedRoomId, via: Vec<OwnedServerName>) -> Self {
        Self {
            kind: CONFERENCE_TYPE_RTC.to_owned(),
            application: CONFERENCE_APPLICATION_CALL.to_owned(),
            room_id,
            slot_id: None,
            via,
            label: None,
        }
    }

    /// Sets the slot ID.
    pub fn with_slot_id(mut self, slot_id: impl Into<String>) -> Self {
        self.slot_id = Some(slot_id.into());
        self
    }

    /// Sets the label.
    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }

    /// Whether this is a MatrixRTC (`m.rtc`) conference.
    pub fn is_rtc(&self) -> bool {
        self.kind == CONFERENCE_TYPE_RTC
    }

    /// Whether the room can be resolved, i.e. at least one `via` server is given.
    ///
    /// Clients should disable the join action when this returns `false`.
    pub fn is_resolvable(&self) -> bool {
        !self.via.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use matrix_sdk::ruma::{owned_room_id, owned_server_name};
    use serde_json::json;

    use super::*;

    #[test]
    fn msc_example_round_trip() {
        let v = json!({
          "type": "m.rtc",
          "application": "m.call",
          "room_id": "!callroom:example.com",
          "slot_id": "...",
          "via": ["example.com", "matrix.org"],
          "label": "Join call"
        });
        let c: Conference = serde_json::from_value(v.clone()).unwrap();
        assert!(c.is_resolvable());
        assert!(c.is_rtc());
        let built = Conference::call(
            owned_room_id!("!callroom:example.com"),
            vec![
                owned_server_name!("example.com"),
                owned_server_name!("matrix.org"),
            ],
        )
        .with_slot_id("...")
        .with_label("Join call");
        assert_eq!(built, c);
        assert_eq!(serde_json::to_value(&built).unwrap(), v);
    }

    #[test]
    fn missing_via_is_unresolvable() {
        let c: Conference = serde_json::from_value(json!({
          "type": "m.rtc",
          "application": "m.call",
          "room_id": "!callroom:example.com"
        }))
        .unwrap();
        assert!(!c.is_resolvable());
    }
}
