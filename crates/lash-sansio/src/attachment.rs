use std::fmt;
use std::str::FromStr;

#[derive(
    Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize, PartialOrd, Ord,
)]
#[serde(transparent)]
pub struct AttachmentId(String);

impl AttachmentId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AttachmentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for AttachmentId {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl From<&str> for AttachmentId {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InvalidMediaType {
    value: String,
}

impl fmt::Display for InvalidMediaType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid media type `{}`: expected a syntactically valid type/subtype",
            self.value
        )
    }
}

impl std::error::Error for InvalidMediaType {}

/// A syntactically validated MIME media type.
///
/// Lash deliberately does not maintain a closed media catalog. Provider
/// adapters own the MIME families and exact values they can materialize.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MediaType(String);

impl MediaType {
    pub fn is_image(&self) -> bool {
        self.family() == "image"
    }

    pub fn parse(value: impl AsRef<str>) -> Result<Self, InvalidMediaType> {
        let original = value.as_ref();
        let normalized = original.trim().to_ascii_lowercase();
        let mut pieces = normalized.split('/');
        let type_name = pieces.next().unwrap_or_default();
        let subtype = pieces.next().unwrap_or_default();
        if pieces.next().is_some() || !is_mime_token(type_name) || !is_mime_token(subtype) {
            return Err(InvalidMediaType {
                value: original.to_string(),
            });
        }
        Ok(Self(normalized))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn family(&self) -> &str {
        self.0.split_once('/').map_or("", |(family, _)| family)
    }
}

fn is_mime_token(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        })
}

impl fmt::Display for MediaType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for MediaType {
    type Err = InvalidMediaType;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl TryFrom<String> for MediaType {
    type Error = InvalidMediaType;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl serde::Serialize for MediaType {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> serde::Deserialize<'de> for MediaType {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum AttachmentTypeMetadata {
    Image {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        width: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        height: Option<u32>,
    },
}

impl AttachmentTypeMetadata {
    pub fn image(width: Option<u32>, height: Option<u32>) -> Self {
        Self::Image { width, height }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AttachmentCreateMeta {
    pub media_type: MediaType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub type_metadata: Option<AttachmentTypeMetadata>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

impl AttachmentCreateMeta {
    pub fn new(
        media_type: MediaType,
        type_metadata: Option<AttachmentTypeMetadata>,
        label: Option<String>,
    ) -> Self {
        Self {
            media_type,
            type_metadata,
            label,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AttachmentMeta {
    pub id: AttachmentId,
    pub media_type: MediaType,
    pub byte_len: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub type_metadata: Option<AttachmentTypeMetadata>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

impl AttachmentMeta {
    pub fn new(
        id: AttachmentId,
        media_type: MediaType,
        byte_len: u64,
        type_metadata: Option<AttachmentTypeMetadata>,
        label: Option<String>,
    ) -> Self {
        Self {
            id,
            media_type,
            byte_len,
            type_metadata,
            label,
        }
    }

    pub fn as_ref(&self) -> AttachmentRef {
        AttachmentRef {
            id: self.id.clone(),
            media_type: self.media_type.clone(),
            byte_len: self.byte_len,
            type_metadata: self.type_metadata.clone(),
            label: self.label.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AttachmentRef {
    pub id: AttachmentId,
    pub media_type: MediaType,
    pub byte_len: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub type_metadata: Option<AttachmentTypeMetadata>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

impl AttachmentRef {
    pub fn media_type(&self) -> &MediaType {
        &self.media_type
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_type_accepts_any_valid_type_and_normalizes_case() {
        assert_eq!(
            MediaType::parse(" IMAGE/PNG ").unwrap().as_str(),
            "image/png"
        );
        assert_eq!(
            MediaType::parse("application/vnd.example+json")
                .unwrap()
                .as_str(),
            "application/vnd.example+json"
        );
        assert!(MediaType::parse("application/x.foo~bar").is_ok());
    }

    #[test]
    fn media_type_rejects_parameters_and_malformed_values() {
        for invalid in [
            "image",
            "/png",
            "image/",
            "image/png/extra",
            "text/plain; charset=utf-8",
        ] {
            assert!(MediaType::parse(invalid).is_err(), "accepted {invalid}");
        }
    }

    #[test]
    fn serde_cannot_bypass_media_type_validation() {
        assert!(serde_json::from_str::<MediaType>(r#""not a mime""#).is_err());
    }
}
