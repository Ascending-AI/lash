//! Bounds on execution evidence, diagnostics and working memory: what one
//! tool attempt's recorded stream, an effect diff, a failed generation's
//! output and a process node's occurrences keep; how much of a diagnostic
//! survives; and how much composition history a runtime remembers.
//!
//! None of these is telemetry: they bound what lash records and keeps, not
//! what it exports. Each is a sealed policy with a documented default and
//! range per field, refused with a typed `{ field, value, min, max }` error.

use std::num::NonZeroU64;

/// How many effect occurrences of one node are recorded one by one, across
/// every site of the node: the hard maximum of 4096.
/// The host chooses an evidence cut through
/// [`EvidenceLimits`] (default 64); the driver pins
/// that cut in its first transition and omissions record it.
pub const MAX_PROCESS_EFFECT_OCCURRENCES: u64 = 4096;

const KIB: u64 = 1024;
const MIB: u64 = 1024 * 1024;

/// A numeric field's documented default and inclusive range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LimitBounds {
    pub default: u64,
    pub min: u64,
    pub max: u64,
}

macro_rules! limits_policy {
    (
        $(#[$meta:meta])*
        policy $policy:ident, field $field_enum:ident, error $error:ident,
        builder $builder:ident, prefix $prefix:literal {
            $($field:ident: $ty:ty => $id:ident ($default:expr, $min:expr, $max:expr), $doc:literal;)+
        }
    ) => {
        /// A field of the policy, as its error names it.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        #[non_exhaustive]
        pub enum $field_enum { $($id),+ }

        impl $field_enum {
            /// Every field, in table order.
            pub const ALL: &[Self] = &[$(Self::$id),+];

            /// The field's documented default and range.
            pub const fn bounds(self) -> LimitBounds {
                match self {
                    $(Self::$id => LimitBounds { default: $default, min: $min, max: $max }),+
                }
            }

            fn name(self) -> &'static str {
                match self { $(Self::$id => concat!($prefix, ".", stringify!($field))),+ }
            }

            fn check(self, value: u64) -> Result<(), $error> {
                let LimitBounds { min, max, .. } = self.bounds();
                if (min..=max).contains(&value) {
                    Ok(())
                } else {
                    Err($error::Range { field: self, value, min, max })
                }
            }
        }

        impl std::fmt::Display for $field_enum {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.name())
            }
        }

        /// A value outside its field's documented range.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
        #[non_exhaustive]
        pub enum $error {
            #[error("{field} = {value} is outside {min}..={max}")]
            Range { field: $field_enum, value: u64, min: u64, max: u64 },
        }

        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub struct $policy {
            $($field: $ty,)+
        }

        #[allow(clippy::cast_possible_truncation, reason = "every default fits its field")]
        impl $policy {
            /// Every field at its documented default.
            pub const fn standard() -> Self {
                Self { $($field: $default as $ty,)+ }
            }

            pub fn builder() -> $builder {
                $builder { $($field: $default,)+ }
            }

            $(
                #[doc = $doc]
                pub const fn $field(&self) -> $ty {
                    self.$field
                }
            )+
        }

        impl Default for $policy {
            fn default() -> Self {
                Self::standard()
            }
        }

        /// Builds the policy from its standard values.
        pub struct $builder {
            $($field: u64,)+
        }

        impl $builder {
            $(
                #[doc = $doc]
                #[must_use]
                pub fn $field(mut self, value: u64) -> Self {
                    self.$field = value;
                    self
                }
            )+

            /// Refuses the first field, in table order, outside its range.
            #[allow(clippy::cast_possible_truncation, reason = "each value was checked against a range that fits its field")]
            pub fn build(self) -> Result<$policy, $error> {
                $($field_enum::$id.check(self.$field)?;)+
                Ok($policy { $($field: self.$field as $ty,)+ })
            }
        }
    };
}

limits_policy! {
    /// The evidence lash records about execution: tool attempt streams,
    /// effect diffs, failed generations and process effect occurrences.
    policy EvidenceLimits, field EvidenceLimitsField, error EvidenceLimitsError,
    builder EvidenceLimitsBuilder, prefix "evidence_limits" {
        attempt_stream_bytes: usize => AttemptStreamBytes(256 * KIB, 0, 16 * MIB),
            "Recorded payload bytes of one tool attempt's stream (default 256 KiB, 0..=16 MiB).";
        diff_value_json_bytes: usize => DiffValueJsonBytes(2048, 0, MIB),
            "Largest whole JSON value an effect diff keeps (default 2048 B, 0..=1 MiB).";
        diff_summary_paths: usize => DiffSummaryPaths(8, 0, 4096),
            "Divergent paths in a mismatch's error summary (default 8, 0..=4096).";
        failure_partial_output_bytes: usize => FailurePartialOutputBytes(64 * KIB, 0, 8 * MIB),
            "Visible output kept from a failed generation, its truncation marker included (default 64 KiB, 0..=8 MiB).";
        process_effect_occurrences: u64 => ProcessEffectOccurrences(
            64, 0, MAX_PROCESS_EFFECT_OCCURRENCES),
            "Effect occurrences of one process node recorded one by one, pinned by the process; zero summarizes every occurrence (default 64, 0..=4096).";
    }
}

limits_policy! {
    /// How much composition history a runtime remembers to report each model
    /// call's tool composition once.
    policy CompositionCacheLimits, field CompositionCacheLimitsField,
    error CompositionCacheLimitsError, builder CompositionCacheLimitsBuilder,
    prefix "composition_cache_limits" {
        fingerprint_generations: usize => FingerprintGenerations(8, 0, 4096),
            "Cached tool-composition fingerprint generations; zero disables the cache (default 8, 0..=4096).";
        sessions: usize => Sessions(1024, 0, 65_536),
            "Sessions whose last reported composition is remembered; zero remembers none, so every model call reports its composition (default 1024, 0..=65536).";
    }
}

/// A field of [`DiagnosticLimits`], as its error names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum DiagnosticLimitsField {
    DiagnosticErrorChars,
    PluginStateWarnBytes,
}

impl DiagnosticLimitsField {
    /// Every field, in table order.
    pub const ALL: &[Self] = &[Self::DiagnosticErrorChars, Self::PluginStateWarnBytes];

    /// The field's documented default and range. The plugin-state warning is
    /// also off when unset.
    pub const fn bounds(self) -> LimitBounds {
        match self {
            Self::DiagnosticErrorChars => LimitBounds {
                default: 4000,
                min: 0,
                max: MIB,
            },
            Self::PluginStateWarnBytes => LimitBounds {
                default: crate::plugin_state::PLUGIN_STATE_SESSION_WARN as u64,
                min: 1,
                max: 64 * MIB,
            },
        }
    }

    fn check(self, value: u64) -> Result<(), DiagnosticLimitsError> {
        let LimitBounds { min, max, .. } = self.bounds();
        if (min..=max).contains(&value) {
            Ok(())
        } else {
            Err(DiagnosticLimitsError::Range {
                field: self,
                value,
                min,
                max,
            })
        }
    }
}

impl std::fmt::Display for DiagnosticLimitsField {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::DiagnosticErrorChars => "diagnostic_limits.diagnostic_error_chars",
            Self::PluginStateWarnBytes => "diagnostic_limits.plugin_state_warn_bytes",
        })
    }
}

/// A value outside its field's documented range.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum DiagnosticLimitsError {
    #[error("{field} = {value} is outside {min}..={max}")]
    Range {
        field: DiagnosticLimitsField,
        value: u64,
        min: u64,
        max: u64,
    },
}

/// How much of a diagnostic lash keeps, and when it warns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiagnosticLimits {
    diagnostic_error_chars: usize,
    plugin_state_warn_bytes: Option<NonZeroU64>,
}

#[allow(
    clippy::cast_possible_truncation,
    reason = "every default fits its field"
)]
impl DiagnosticLimits {
    /// 4000 diagnostic characters and a 4 MiB plugin-state warning.
    pub const fn standard() -> Self {
        Self {
            diagnostic_error_chars: DiagnosticLimitsField::DiagnosticErrorChars.bounds().default
                as usize,
            plugin_state_warn_bytes: NonZeroU64::new(
                DiagnosticLimitsField::PluginStateWarnBytes.bounds().default,
            ),
        }
    }

    pub fn builder() -> DiagnosticLimitsBuilder {
        let standard = Self::standard();
        DiagnosticLimitsBuilder {
            diagnostic_error_chars: standard.diagnostic_error_chars as u64,
            plugin_state_warn_bytes: standard.plugin_state_warn_bytes.map(NonZeroU64::get),
        }
    }

    /// Characters kept from a diagnostic before its omission marker
    /// (default 4000, 0..=1 048 576).
    pub const fn diagnostic_error_chars(&self) -> usize {
        self.diagnostic_error_chars
    }

    /// The encoded plugin-state size past which a committed publication is
    /// reported; `None` turns the warning off (default 4 MiB, 1 B..=64 MiB).
    /// The publication's rejection budget is independent.
    pub const fn plugin_state_warn_bytes(&self) -> Option<NonZeroU64> {
        self.plugin_state_warn_bytes
    }

    /// Keeps a diagnostic's head and tail, counting original Unicode
    /// characters.
    pub fn diagnostic_error(self, text: &str) -> String {
        let chars = text.chars().count();
        if chars <= self.diagnostic_error_chars {
            return text.to_owned();
        }
        let keep = self.diagnostic_error_chars / 2;
        let head = text.chars().take(keep).collect::<String>();
        let tail = text
            .chars()
            .rev()
            .take(keep)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<String>();
        let omitted = chars - keep * 2;
        format!("{head}\n\n... ({omitted} chars omitted) ...\n\n{tail}")
    }
}

impl Default for DiagnosticLimits {
    fn default() -> Self {
        Self::standard()
    }
}

/// Builds a [`DiagnosticLimits`] from its standard values.
pub struct DiagnosticLimitsBuilder {
    diagnostic_error_chars: u64,
    plugin_state_warn_bytes: Option<u64>,
}

impl DiagnosticLimitsBuilder {
    /// Characters kept from a diagnostic before its omission marker.
    #[must_use]
    pub fn diagnostic_error_chars(mut self, value: u64) -> Self {
        self.diagnostic_error_chars = value;
        self
    }

    /// The plugin-state warning threshold, or `None` for no warning.
    #[must_use]
    pub fn plugin_state_warn_bytes(mut self, value: Option<u64>) -> Self {
        self.plugin_state_warn_bytes = value;
        self
    }

    /// Refuses the first field, in table order, outside its range.
    pub fn build(self) -> Result<DiagnosticLimits, DiagnosticLimitsError> {
        DiagnosticLimitsField::DiagnosticErrorChars.check(self.diagnostic_error_chars)?;
        if let Some(bytes) = self.plugin_state_warn_bytes {
            DiagnosticLimitsField::PluginStateWarnBytes.check(bytes)?;
        }
        Ok(DiagnosticLimits {
            diagnostic_error_chars: usize::try_from(self.diagnostic_error_chars)
                .unwrap_or(usize::MAX),
            plugin_state_warn_bytes: self.plugin_state_warn_bytes.and_then(NonZeroU64::new),
        })
    }
}
