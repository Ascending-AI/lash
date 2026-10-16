//! Offline validation of the host's rendered JSON configuration.

use super::PostgresHostConfig;

/// Why a rendered host configuration was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostConfigErrorKind {
    /// A required field is absent.
    Missing,
    /// An object contains an unrecognized field.
    Unknown,
    /// Invalid JSON, a wrong type, or a value that breaks a configuration rule.
    Invalid,
}

/// A refusal before any node or connection is opened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostConfigError {
    /// Dot-separated JSON field path; `$` denotes the document itself.
    pub path: String,
    /// The category hosts can match without parsing the diagnostic.
    pub kind: HostConfigErrorKind,
    /// The parser or configuration validator's diagnostic.
    pub reason: String,
}

impl std::fmt::Display for HostConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.path, self.reason)
    }
}

impl std::error::Error for HostConfigError {}

/// Validate rendered PostgreSQL host configuration without starting a node.
///
/// # Errors
/// Returns the first parser or configuration refusal with its JSON field path.
pub fn validate_host_config(json: &str) -> Result<(), HostConfigError> {
    let mut deserializer = serde_json::Deserializer::from_str(json);
    let config: PostgresHostConfig =
        serde_path_to_error::deserialize(&mut deserializer).map_err(parse_error)?;
    // deserialize() reads one value; from_str() also refuses trailing input.
    deserializer.end().map_err(|error| HostConfigError {
        path: "$".to_owned(),
        kind: HostConfigErrorKind::Invalid,
        reason: error.to_string(),
    })?;
    config
        .validate()
        .map(|_| ())
        .map_err(|error| HostConfigError {
            path: error.field,
            kind: HostConfigErrorKind::Invalid,
            reason: error.reason,
        })
}

fn parse_error(error: serde_path_to_error::Error<serde_json::Error>) -> HostConfigError {
    let mut path = error.path().to_string();
    if error
        .path()
        .iter()
        .all(|segment| matches!(segment, serde_path_to_error::Segment::Unknown))
    {
        path.clear();
    }
    let reason = error.inner().to_string();
    // serde_json exposes serde's missing/unknown-field diagnostics as text.
    // The field names here are generated Rust identifiers; only these two
    // standard serde diagnostics are classified, all others stay Invalid.
    let kind = if let Some(field) = diagnostic_field(&reason, "missing field `") {
        if !path.is_empty() {
            path.push('.');
        }
        path.push_str(field);
        HostConfigErrorKind::Missing
    } else if diagnostic_field(&reason, "unknown field `").is_some() {
        // The rejected map key is already included by serde_path_to_error.
        HostConfigErrorKind::Unknown
    } else {
        HostConfigErrorKind::Invalid
    };
    HostConfigError {
        path: if path.is_empty() {
            "$".to_owned()
        } else {
            path
        },
        kind,
        reason,
    }
}

fn diagnostic_field<'a>(reason: &'a str, prefix: &str) -> Option<&'a str> {
    reason
        .strip_prefix(prefix)?
        .split_once('`')
        .map(|(field, _)| field)
}
