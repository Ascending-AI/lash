//! The version range every compatibility surface declares (ADR 0115 §1.1).
//!
//! A stored component, the fleet epoch `F`, the remote protocol and the
//! Restate handler wire all state what they support as one inclusive range,
//! and two builds agree on a version by [`VersionRange::select`]: the highest
//! version both ranges contain. The JSON shape `{"min":1,"max":1}` is frozen:
//! every build parses every peer's range, so it never gains a field.

use serde::{Deserialize, Serialize};

/// A non-empty inclusive range of versions of one surface. JSON
/// `{"min":1,"max":1}`, frozen: every build parses every range.
///
/// The fields are private so a range can only be built through
/// [`VersionRange::new`], [`VersionRange::between`] or
/// [`VersionRange::exactly`], and a decoded range is
/// validated the same way: version 0 names nothing, and `min > max` is empty.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "RawVersionRange", into = "RawVersionRange")]
pub struct VersionRange {
    min: u32,
    max: u32,
}

/// The wire form of [`VersionRange`], before validation.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[schemars(rename = "VersionRange")]
struct RawVersionRange {
    #[schemars(range(min = 1))]
    min: u32,
    #[schemars(range(min = 1))]
    max: u32,
}

/// Why a pair of bounds is not a [`VersionRange`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum VersionRangeError {
    /// Version 0 is never a version: every surface starts at 1.
    ZeroMin,
    /// `min > max`: the range contains no version.
    Empty { min: u32, max: u32 },
}

impl std::fmt::Display for VersionRangeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ZeroMin => formatter.write_str("a version range cannot start at version 0"),
            Self::Empty { min, max } => {
                write!(formatter, "version range [{min},{max}] is empty")
            }
        }
    }
}

impl std::error::Error for VersionRangeError {}

impl VersionRange {
    /// The inclusive range `[min, max]` for declared version surfaces.
    ///
    /// Panics at compile time in a `const` if either bound is zero or the
    /// range is empty.
    pub const fn between(min: u32, max: u32) -> Self {
        assert!(
            min != 0 && max != 0,
            "a version range cannot contain version 0"
        );
        assert!(min <= max, "a version range cannot be empty");
        Self { min, max }
    }

    /// The one-version range `[version, version]`.
    ///
    /// Panics at compile time (in a `const`) or at run time when `version` is
    /// 0; every declared surface starts at 1.
    pub const fn exactly(version: u32) -> Self {
        assert!(version != 0, "a version range cannot contain version 0");
        Self {
            min: version,
            max: version,
        }
    }

    /// Refuses `min == 0` and `min > max`.
    pub fn new(min: u32, max: u32) -> Result<Self, VersionRangeError> {
        if min == 0 {
            return Err(VersionRangeError::ZeroMin);
        }
        if min > max {
            return Err(VersionRangeError::Empty { min, max });
        }
        Ok(Self { min, max })
    }

    /// The oldest version the range contains.
    pub const fn min(self) -> u32 {
        self.min
    }

    /// The newest version the range contains.
    pub const fn max(self) -> u32 {
        self.max
    }

    /// Whether `version` is inside the range.
    pub const fn contains(self, version: u32) -> bool {
        self.min <= version && version <= self.max
    }

    /// The highest version both ranges contain; `None` when disjoint.
    pub fn select(self, peer: Self) -> Option<u32> {
        let low = self.min.max(peer.min);
        let high = self.max.min(peer.max);
        (low <= high).then_some(high)
    }
}

impl std::fmt::Display for VersionRange {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "[{},{}]", self.min, self.max)
    }
}

impl TryFrom<RawVersionRange> for VersionRange {
    type Error = VersionRangeError;

    fn try_from(raw: RawVersionRange) -> Result<Self, Self::Error> {
        Self::new(raw.min, raw.max)
    }
}

impl From<VersionRange> for RawVersionRange {
    fn from(range: VersionRange) -> Self {
        Self {
            min: range.min,
            max: range.max,
        }
    }
}

impl schemars::JsonSchema for VersionRange {
    fn schema_name() -> String {
        "VersionRange".to_owned()
    }

    fn json_schema(generator: &mut schemars::r#gen::SchemaGenerator) -> schemars::schema::Schema {
        RawVersionRange::json_schema(generator)
    }
}

#[cfg(test)]
mod tests {
    use super::{VersionRange, VersionRangeError};

    fn range(min: u32, max: u32) -> VersionRange {
        VersionRange::new(min, max).expect("valid range")
    }

    #[test]
    fn version_range_select_is_the_highest_common_version() {
        // A 1.1 peer offering [1,2] and a 1.0 peer offering [1,1] select 1.
        assert_eq!(range(1, 2).select(range(1, 1)), Some(1));
        assert_eq!(range(1, 1).select(range(1, 2)), Some(1));
        // Overlap picks the top of the intersection, from either side.
        assert_eq!(range(1, 3).select(range(2, 5)), Some(3));
        assert_eq!(range(2, 5).select(range(1, 3)), Some(3));
        assert_eq!(range(2, 4).select(range(1, 9)), Some(4));
        assert_eq!(range(3, 3).select(range(3, 3)), Some(3));
        // Touching ranges share their one boundary version.
        assert_eq!(range(1, 2).select(range(2, 3)), Some(2));
        // Disjoint ranges select nothing.
        assert_eq!(range(1, 1).select(range(2, 2)), None);
        assert_eq!(range(4, 6).select(range(1, 3)), None);
    }

    #[test]
    fn version_range_refuses_empty_and_zero() {
        assert_eq!(VersionRange::new(0, 0), Err(VersionRangeError::ZeroMin));
        assert_eq!(VersionRange::new(0, 3), Err(VersionRangeError::ZeroMin));
        assert_eq!(
            VersionRange::new(3, 2),
            Err(VersionRangeError::Empty { min: 3, max: 2 })
        );
        assert_eq!(VersionRange::new(2, 2), Ok(VersionRange::exactly(2)));

        // Decoding validates exactly as construction does.
        for refused in [
            r#"{"min":0,"max":1}"#,
            r#"{"min":2,"max":1}"#,
            r#"{"min":1}"#,
        ] {
            assert!(
                serde_json::from_str::<VersionRange>(refused).is_err(),
                "{refused} must not decode"
            );
        }

        let decoded: VersionRange =
            serde_json::from_str(r#"{"min":1,"max":2}"#).expect("valid range decodes");
        assert_eq!(decoded, range(1, 2));
        assert_eq!(
            serde_json::to_string(&range(1, 2)).expect("encode"),
            r#"{"min":1,"max":2}"#
        );
        assert!(range(1, 2).contains(1) && range(1, 2).contains(2));
        assert!(!range(1, 2).contains(0) && !range(1, 2).contains(3));
    }

    #[test]
    fn between_constructs_a_const_two_version_range() {
        const TWO: VersionRange = VersionRange::between(1, 2);
        assert_eq!(TWO, range(1, 2));
        assert_eq!(TWO.select(VersionRange::exactly(2)), Some(2));
    }

    #[test]
    fn between_refuses_invalid_bounds() {
        for (min, max) in [(0, 0), (0, 2), (2, 0), (3, 2)] {
            assert!(std::panic::catch_unwind(|| VersionRange::between(min, max)).is_err());
        }
    }
}
