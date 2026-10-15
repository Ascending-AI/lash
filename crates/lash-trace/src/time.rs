//! Lash's own time type on the trace surface.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A point in time on lash's trace surface: milliseconds since the Unix
/// epoch, UTC.
///
/// Serialized as an RFC 3339 string at millisecond precision, the form
/// trace records have always carried. A value before the epoch is a decode
/// error.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TraceTimestamp(u64);

impl TraceTimestamp {
    pub const fn from_unix_ms(ms: u64) -> Self {
        Self(ms)
    }

    pub const fn unix_ms(self) -> u64 {
        self.0
    }

    pub fn to_system_time(self) -> std::time::SystemTime {
        std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_millis(self.0)
    }

    /// `None` before the epoch or past `u64` milliseconds.
    pub fn from_system_time(at: std::time::SystemTime) -> Option<Self> {
        let since = at.duration_since(std::time::SystemTime::UNIX_EPOCH).ok()?;
        u64::try_from(since.as_millis()).ok().map(Self)
    }

    /// Saturating: zero when `earlier` is later.
    pub fn duration_since(self, earlier: Self) -> std::time::Duration {
        std::time::Duration::from_millis(self.0.saturating_sub(earlier.0))
    }

    pub(crate) fn to_chrono(self) -> chrono::DateTime<chrono::Utc> {
        i64::try_from(self.0)
            .ok()
            .and_then(chrono::DateTime::from_timestamp_millis)
            .unwrap_or(chrono::DateTime::<chrono::Utc>::MAX_UTC)
    }
}

impl std::fmt::Display for TraceTimestamp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_chrono().to_rfc3339())
    }
}

impl Serialize for TraceTimestamp {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_chrono().to_rfc3339())
    }
}

impl<'de> Deserialize<'de> for TraceTimestamp {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        let at = chrono::DateTime::parse_from_rfc3339(&text).map_err(serde::de::Error::custom)?;
        u64::try_from(at.timestamp_millis())
            .map(Self)
            .map_err(|_| serde::de::Error::custom("a trace timestamp precedes the Unix epoch"))
    }
}

impl schemars::JsonSchema for TraceTimestamp {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "TraceTimestamp".into()
    }

    fn json_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({ "type": "string", "format": "date-time" })
    }
}

#[cfg(test)]
mod tests {
    use super::TraceTimestamp;

    /// The wire form is the RFC 3339 text chrono wrote before lash owned the
    /// type, so a reader of either build reads the other's records.
    #[test]
    fn the_wire_form_is_the_rfc3339_text_records_always_carried() {
        for ms in [0_u64, 1_700_000_000_000, 1_700_000_000_123] {
            let at = TraceTimestamp::from_unix_ms(ms);
            let chrono = chrono::DateTime::from_timestamp_millis(ms as i64).unwrap();
            let wire = serde_json::to_value(at).unwrap();
            assert_eq!(wire, serde_json::json!(chrono.to_rfc3339()));
            assert_eq!(serde_json::from_value::<TraceTimestamp>(wire).unwrap(), at);
        }
        assert!(
            serde_json::from_value::<TraceTimestamp>(serde_json::json!(
                "1969-12-31T23:59:59+00:00"
            ))
            .is_err()
        );
    }
}
