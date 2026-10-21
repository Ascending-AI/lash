//! Injective, context-free spellings of canonical surface segments.

use lash_vocabulary::names::Segment;

/// The syntactic namespace of a surface name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpellKind {
    PathSegment,
    /// Operations, functions and members of value types.
    Member,
    /// Exported types and errors.
    TypeName,
}

/// Each dialect's sole mapping between canonical segments and source names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Spelling {
    TypeScript,
    Python,
}

impl Spelling {
    pub fn spell(self, kind: SpellKind, segment: &Segment) -> String {
        if self.verbatim(kind) {
            return segment.as_str().to_owned();
        }
        let mut upper = kind == SpellKind::TypeName;
        let mut text = String::with_capacity(segment.as_str().len());
        for byte in segment.as_str().bytes() {
            if byte == b'_' {
                upper = true;
            } else {
                text.push(char::from(if upper {
                    byte.to_ascii_uppercase()
                } else {
                    byte
                }));
                upper = false;
            }
        }
        text
    }

    /// Return the unique canonical segment spelling `text`, with no registry,
    /// environment or dialect state involved.
    pub fn unspell(self, kind: SpellKind, text: &str) -> Option<Segment> {
        if self.verbatim(kind) {
            return Segment::new(text).ok();
        }
        let mut bytes = text.bytes();
        let first = bytes.next()?;
        if (kind == SpellKind::TypeName && !first.is_ascii_uppercase())
            || (kind == SpellKind::Member && !first.is_ascii_lowercase())
        {
            return None;
        }
        let mut canonical = String::with_capacity(text.len());
        canonical.push(char::from(first.to_ascii_lowercase()));
        for byte in bytes {
            if byte.is_ascii_uppercase() {
                canonical.push('_');
                canonical.push(char::from(byte.to_ascii_lowercase()));
            } else if byte.is_ascii_lowercase() || byte.is_ascii_digit() {
                canonical.push(char::from(byte));
            } else {
                return None;
            }
        }
        Segment::new(canonical).ok()
    }

    fn verbatim(self, kind: SpellKind) -> bool {
        kind == SpellKind::PathSegment || (self == Self::Python && kind == SpellKind::Member)
    }
}
