use std::fmt;
use std::str::FromStr;

/// An attachment identifier failed validation.
///
/// [`AttachmentId::parse`] returns this error when the input is empty, exceeds
/// the length bound, contains non-printable ASCII or path syntax, or cannot
/// serve as a single namespace component.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InvalidAttachmentId {
    value: String,
}

impl fmt::Display for InvalidAttachmentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid attachment id `{}`: expected 1..={MAX_ATTACHMENT_ID_LEN} printable ASCII \
             characters forming a single namespace component",
            self.value.escape_debug()
        )
    }
}

impl std::error::Error for InvalidAttachmentId {}

/// Maximum attachment-id length.
///
/// Content ids minted by Lash are 64-byte lowercase SHA-256 hex strings. The
/// larger bound leaves room for compatible caller-defined ids while keeping
/// file names comfortably below common per-component limits after a staging
/// suffix is appended.
const MAX_ATTACHMENT_ID_LEN: usize = 128;

/// An attachment id, validated at construction.
///
/// Every attachment backend maps this id into a namespace it does not fully
/// control — a filesystem path component, an object-store key segment, a SQL
/// identifier column. An id therefore has to be a *single* namespace component:
/// non-empty, bounded, printable ASCII, free of path separators, not a relative
/// directory reference, and not a drive-qualified path. Ids arrive from places
/// Lash does not control (remote-protocol peers, host HTTP routes, model
/// output), so the check lives at construction: there is no way to obtain an
/// `AttachmentId` that a backend would have to defend itself against, and no
/// silent acceptance of a malformed id that only misbehaves later at the store.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AttachmentId(String);

impl AttachmentId {
    pub fn parse(id: impl AsRef<str>) -> Result<Self, InvalidAttachmentId> {
        let value = id.as_ref();
        let bytes = value.as_bytes();
        let has_windows_drive_prefix =
            bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':';
        let malformed = value.is_empty()
            || value.len() > MAX_ATTACHMENT_ID_LEN
            || !bytes
                .iter()
                .all(|byte| byte.is_ascii_graphic() || *byte == b' ')
            || value.contains(['/', '\\'])
            || matches!(value, "." | "..")
            || has_windows_drive_prefix;

        if malformed {
            return Err(InvalidAttachmentId {
                value: value.to_string(),
            });
        }
        Ok(Self(value.to_string()))
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

impl FromStr for AttachmentId {
    type Err = InvalidAttachmentId;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl TryFrom<String> for AttachmentId {
    type Error = InvalidAttachmentId;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl serde::Serialize for AttachmentId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> serde::Deserialize<'de> for AttachmentId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(value).map_err(serde::de::Error::custom)
    }
}

/// A MIME media type failed syntactic validation.
///
/// [`MediaType::parse`] returns this error when the input is not exactly two
/// non-empty MIME token components separated by one slash.
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
    fn attachment_id_accepts_content_hashes_and_caller_ids() {
        assert_eq!(
            AttachmentId::parse("a".repeat(64)).unwrap().as_str(),
            "a".repeat(64)
        );
        assert_eq!(
            AttachmentId::parse("workbench attachment.png")
                .unwrap()
                .as_str(),
            "workbench attachment.png"
        );
        assert_eq!(
            AttachmentId::parse("a".repeat(MAX_ATTACHMENT_ID_LEN))
                .unwrap()
                .as_str()
                .len(),
            MAX_ATTACHMENT_ID_LEN
        );
    }

    #[test]
    fn attachment_id_rejects_ids_that_are_not_a_single_namespace_component() {
        for invalid in [
            "",
            ".",
            "..",
            "../outside",
            "..\\outside",
            "/etc/passwd",
            "nested/id",
            "C:\\windows",
            "line\nbreak",
            "null\0byte",
            "tab\there",
            &"a".repeat(MAX_ATTACHMENT_ID_LEN + 1),
        ] {
            assert!(
                AttachmentId::parse(invalid).is_err(),
                "accepted {invalid:?}"
            );
        }
    }

    #[test]
    fn serde_cannot_bypass_attachment_id_validation() {
        // Before validation moved to construction this deserialized happily
        // into a well-formed-looking id that only escaped the store root later.
        let error = serde_json::from_str::<AttachmentId>(r#""../../etc/passwd""#)
            .expect_err("traversal id must not deserialize");
        assert!(
            error.to_string().contains("invalid attachment id"),
            "unexpected error: {error}"
        );
        assert_eq!(
            serde_json::from_str::<AttachmentId>(r#""abc123""#).unwrap(),
            AttachmentId::parse("abc123").unwrap()
        );
    }

    #[test]
    fn attachment_id_round_trips_through_json_as_a_bare_string() {
        let id = AttachmentId::parse("abc123").unwrap();
        assert_eq!(serde_json::to_string(&id).unwrap(), r#""abc123""#);
    }

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
