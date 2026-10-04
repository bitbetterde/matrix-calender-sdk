//! Event location (MSC4496 §2.3).

use serde::{Deserialize, Serialize};

string_enum! {
    /// The kind of a [`Location`].
    pub enum LocationType {
        /// A physical place; `uri` is an optional `geo:` URI (RFC 5870).
        Physical = "physical",
        /// A web meeting that is not a Matrix-native call; `uri` is an `https:` URL.
        Virtual = "virtual",
    }
}

/// Where an event takes place.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Location {
    /// Physical or virtual.
    #[serde(rename = "type")]
    pub kind: LocationType,
    /// Human-readable name or address. Required.
    pub name: String,
    /// A `geo:` URI for physical locations or an `https:` URL for virtual ones.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uri: Option<String>,
}

impl Location {
    /// A physical location, optionally with `(latitude, longitude)` coordinates which are
    /// encoded as a `geo:` URI.
    pub fn physical(name: impl Into<String>, geo: Option<(f64, f64)>) -> Self {
        Self {
            kind: LocationType::Physical,
            name: name.into(),
            uri: geo.map(|(lat, lon)| format!("geo:{lat},{lon}")),
        }
    }

    /// A virtual location (web meeting URL).
    pub fn virtual_(name: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            kind: LocationType::Virtual,
            name: name.into(),
            uri: Some(url.into()),
        }
    }

    /// Parses the `(latitude, longitude)` pair from a `geo:` URI, if present and well-formed.
    ///
    /// Parameters after `;` (such as `u=` uncertainty) and an altitude component are ignored.
    pub fn geo(&self) -> Option<(f64, f64)> {
        let rest = self.uri.as_deref()?.strip_prefix("geo:")?;
        let coords = rest.split(';').next()?;
        let mut parts = coords.split(',');
        let lat = parts.next()?.trim().parse().ok()?;
        let lon = parts.next()?.trim().parse().ok()?;
        Some((lat, lon))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn physical_msc_example() {
        let v = json!({
          "type": "physical",
          "name": "Konferenzraum 3, Beispielstraße 1, 24943 Flensburg",
          "uri": "geo:54.7833,9.4333"
        });
        let loc: Location = serde_json::from_value(v.clone()).unwrap();
        assert_eq!(loc.geo(), Some((54.7833, 9.4333)));
        let built = Location::physical(
            "Konferenzraum 3, Beispielstraße 1, 24943 Flensburg",
            Some((54.7833, 9.4333)),
        );
        assert_eq!(built, loc);
        assert_eq!(serde_json::to_value(&built).unwrap(), v);
    }

    #[test]
    fn virtual_msc_example() {
        let v = json!({
          "type": "virtual",
          "name": "Jitsi Meet",
          "uri": "https://meet.example.com/standup"
        });
        let loc: Location = serde_json::from_value(v.clone()).unwrap();
        assert_eq!(loc.kind, LocationType::Virtual);
        let built = Location::virtual_("Jitsi Meet", "https://meet.example.com/standup");
        assert_eq!(serde_json::to_value(&built).unwrap(), v);
    }

    #[test]
    fn unknown_type_round_trips() {
        let v = json!({ "type": "x-hybrid", "name": "Room 3 + Jitsi" });
        let loc: Location = serde_json::from_value(v.clone()).unwrap();
        assert_eq!(loc.kind, LocationType::Other("x-hybrid".into()));
        assert_eq!(serde_json::to_value(&loc).unwrap(), v);
    }

    #[test]
    fn physical_without_uri() {
        let built = Location::physical("Office", None);
        assert_eq!(
            serde_json::to_value(&built).unwrap(),
            json!({ "type": "physical", "name": "Office" })
        );
        assert_eq!(built.geo(), None);
    }
}
