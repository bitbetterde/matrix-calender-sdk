//! The `calendar_info` state event (MSC4496 §1.1).

use chrono_tz::Tz;
use matrix_sdk::ruma::events::{EmptyStateKey, macros::EventContent, message::TextContentBlock};
use serde::{Deserialize, Serialize};

use super::text::serialize_text_block;
use crate::{Error, Result};

/// Content of the `org.matrix.msc4496.calendar_info` (stable: `m.calendar.info`) state event,
/// sent with an empty state key.
#[derive(Clone, Debug, Serialize, Deserialize, EventContent)]
#[ruma_event(
    type = "org.matrix.msc4496.calendar_info",
    alias = "m.calendar.info",
    kind = State,
    state_key_type = EmptyStateKey
)]
pub struct CalendarInfoEventContent {
    /// Human-readable fallback, usually the calendar's name.
    #[serde(
        rename = "m.text",
        alias = "org.matrix.msc1767.text",
        default,
        serialize_with = "serialize_text_block"
    )]
    pub text: TextContentBlock,

    /// The structured calendar metadata.
    #[serde(
        rename = "org.matrix.msc4496.calendar_info",
        alias = "m.calendar.info",
        default
    )]
    pub info: CalendarInfo,
}

impl CalendarInfoEventContent {
    /// Creates the content with the given display name (used as the `m.text` fallback) and
    /// metadata. Returns an error if the colour is not `#rrggbb`.
    pub fn new(name: impl Into<String>, info: CalendarInfo) -> Result<Self> {
        info.validate()?;
        Ok(Self {
            text: TextContentBlock::plain(name),
            info,
        })
    }

    /// The plain-text fallback, typically the calendar's name.
    pub fn name(&self) -> Option<&str> {
        self.text.find_plain()
    }
}

/// The `calendar_info` content block.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CalendarInfo {
    /// Suggested display colour as `#rrggbb`. Advisory.
    ///
    /// Not validated on deserialization so that a bad value does not make the whole calendar
    /// unreadable; use [`CalendarInfo::valid_color`] when rendering.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,

    /// The calendar's home IANA timezone. Advisory; never overrides per-event timezones.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<Tz>,
}

impl CalendarInfo {
    /// Creates calendar metadata, validating the colour.
    pub fn new(color: Option<String>, timezone: Option<Tz>) -> Result<Self> {
        let info = Self { color, timezone };
        info.validate()?;
        Ok(info)
    }

    /// Checks that `color`, if set, is of the form `#rrggbb`.
    pub fn validate(&self) -> Result<()> {
        match &self.color {
            Some(c) if !is_valid_color(c) => Err(Error::InvalidColor(c.clone())),
            _ => Ok(()),
        }
    }

    /// The colour if it is set and valid.
    pub fn valid_color(&self) -> Option<&str> {
        self.color.as_deref().filter(|c| is_valid_color(c))
    }
}

/// Whether `s` is a colour of the form `#rrggbb` (hex digits, either case).
pub fn is_valid_color(s: &str) -> bool {
    s.len() == 7 && s.starts_with('#') && s.bytes().skip(1).all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use chrono_tz::Europe::Berlin;
    use matrix_sdk::ruma::events::{
        PossiblyRedactedStateEventContent, StateEventType, StaticEventContent,
    };
    use serde_json::json;

    use super::*;

    #[test]
    fn msc_example() {
        let v = json!({
          "type": "m.calendar.info",
          "state_key": "",
          "content": {
            "m.text": [{ "body": "Personal calendar", "mimetype": "text/plain" }],
            "m.calendar.info": {
              "color": "#3c82f6",
              "timezone": "Europe/Berlin"
            }
          }
        });
        let content: CalendarInfoEventContent =
            serde_json::from_value(v["content"].clone()).unwrap();
        assert_eq!(content.name(), Some("Personal calendar"));
        assert_eq!(content.info.valid_color(), Some("#3c82f6"));
        assert_eq!(content.info.timezone, Some(Berlin));
    }

    #[test]
    fn builder_serializes_unstable_names() {
        let content = CalendarInfoEventContent::new(
            "Personal calendar",
            CalendarInfo::new(Some("#3c82f6".into()), Some(Berlin)).unwrap(),
        )
        .unwrap();
        assert_eq!(
            serde_json::to_value(&content).unwrap(),
            json!({
              "m.text": [{ "body": "Personal calendar", "mimetype": "text/plain" }],
              "org.matrix.msc4496.calendar_info": {
                "color": "#3c82f6",
                "timezone": "Europe/Berlin"
              }
            })
        );
        assert_eq!(
            CalendarInfoEventContent::TYPE,
            "org.matrix.msc4496.calendar_info"
        );
        assert_eq!(
            content.event_type(),
            StateEventType::from("org.matrix.msc4496.calendar_info")
        );
    }

    #[test]
    fn color_validation() {
        assert!(is_valid_color("#3c82f6"));
        assert!(is_valid_color("#ABCDEF"));
        assert!(!is_valid_color("3c82f6"));
        assert!(!is_valid_color("#3c82f"));
        assert!(!is_valid_color("#3c82f6a"));
        assert!(!is_valid_color("#3c82g6"));
        assert!(!is_valid_color("#3c8ü"));
        assert!(matches!(
            CalendarInfo::new(Some("blue".into()), None),
            Err(Error::InvalidColor(_))
        ));

        // Invalid colours are tolerated on input but not reported as valid.
        let info: CalendarInfo = serde_json::from_value(json!({ "color": "red" })).unwrap();
        assert_eq!(info.valid_color(), None);
        assert!(info.validate().is_err());
    }

    #[test]
    fn rejects_unknown_timezone() {
        assert!(
            serde_json::from_value::<CalendarInfo>(json!({ "timezone": "Nowhere/City" })).is_err()
        );
    }
}
