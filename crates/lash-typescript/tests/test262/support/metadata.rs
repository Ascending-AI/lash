// Copyright (c) 2019 Jason Williams
// Source: https://github.com/boa-dev/boa at 93a9e31a83bbaa15bbd8b687e61639ffc53bbef1; MIT licensed.
// Local modifications: narrowed Boa's Test262 metadata reader to the fields and errors used by Lash's test runner.

use std::path::Path;

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Metadata {
    pub(crate) description: Box<str>,
    #[serde(default)]
    pub(crate) features: Box<[Box<str>]>,
    #[serde(default)]
    pub(crate) includes: Box<[Box<str>]>,
    #[serde(default)]
    pub(crate) flags: Box<[TestFlag]>,
    #[serde(default)]
    pub(crate) negative: Option<Negative>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Negative {
    pub(crate) phase: Phase,
    #[serde(rename = "type")]
    pub(crate) error_type: ErrorType,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[allow(clippy::enum_variant_names)] // Test262 spells these names and YAML deserialization is exact.
pub(crate) enum ErrorType {
    Test262Error,
    SyntaxError,
    ReferenceError,
    RangeError,
    TypeError,
    EvalError,
}

impl ErrorType {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Test262Error => "Test262Error",
            Self::SyntaxError => "SyntaxError",
            Self::ReferenceError => "ReferenceError",
            Self::RangeError => "RangeError",
            Self::TypeError => "TypeError",
            Self::EvalError => "EvalError",
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum TestFlag {
    OnlyStrict,
    NoStrict,
    Module,
    Raw,
    Async,
    Generated,
    #[serde(rename = "CanBlockIsFalse")]
    CanBlockIsFalse,
    #[serde(rename = "CanBlockIsTrue")]
    CanBlockIsTrue,
    #[serde(rename = "non-deterministic")]
    NonDeterministic,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Phase {
    Parse,
    Resolution,
    Runtime,
}

pub(crate) fn read_metadata(path: &Path) -> Result<Metadata, String> {
    let code = std::fs::read_to_string(path)
        .map_err(|error| format!("could not read {}: {error}", path.display()))?;
    let (_, metadata) = code
        .split_once("/*---")
        .ok_or_else(|| format!("{} has no Test262 frontmatter", path.display()))?;
    let (metadata, _) = metadata
        .split_once("---*/")
        .ok_or_else(|| format!("{} has unterminated Test262 frontmatter", path.display()))?;
    let metadata = metadata.replace('\r', "\n");
    serde_yaml::from_str(&metadata)
        .map_err(|error| format!("invalid Test262 metadata in {}: {error}", path.display()))
}
