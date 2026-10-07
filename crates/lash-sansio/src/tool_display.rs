//! What a host shows of one tool call (FIG-5290, ADR 0129 addendum).
//!
//! A tool declares a [`ToolDisplay`] on its output, a presentation step may
//! replace it, and the presentation boundary journals it, bounded, with the
//! call's outcome. It is committed with the call's transcript record and
//! rendered into the call's row; no model path ever reads it.

use serde::{Deserialize, Serialize};

/// The canonical-JSON byte cap of one call's display. The display is
/// display-only, so a larger one is cut, never refused.
pub const TOOL_DISPLAY_LIMIT_BYTES: usize = 16 * 1024;

/// A tool call's host-facing display: an argument summary, a result summary,
/// and its citations and links, in order.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ToolDisplay {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<ToolDisplayLink>,
    /// Set when the presentation boundary cut this display to its cap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncated: Option<ToolDisplayTruncation>,
}

/// One citation or link of a display. The fields keep the names of
/// [`crate::ToolViewBlock::ResourceLink`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ToolDisplayLink {
    pub uri: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// How large a cut display was, and the cap it was cut to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ToolDisplayTruncation {
    pub original_bytes: u64,
    pub limit_bytes: u64,
}

impl ToolDisplay {
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn with_arguments(mut self, arguments: impl Into<String>) -> Self {
        self.arguments = Some(arguments.into());
        self
    }

    #[must_use]
    pub fn with_result(mut self, result: impl Into<String>) -> Self {
        self.result = Some(result.into());
        self
    }

    #[must_use]
    pub fn with_link(mut self, link: ToolDisplayLink) -> Self {
        self.links.push(link);
        self
    }

    /// This display within `limit` bytes of canonical JSON. One that fits is
    /// returned unchanged. Otherwise links are dropped from the end, then the
    /// result and then the argument summary are cut at a character boundary,
    /// and `truncated` records the original size and the cap. Deterministic,
    /// so a replay cuts the same display the same way.
    #[must_use]
    pub fn bounded(mut self, limit: usize) -> Self {
        let original = encoded_len(&self);
        if original <= limit {
            return self;
        }
        self.truncated = Some(ToolDisplayTruncation {
            original_bytes: original as u64,
            limit_bytes: limit as u64,
        });
        loop {
            let excess = encoded_len(&self).saturating_sub(limit);
            if excess == 0 {
                return self;
            }
            if self.links.pop().is_some() {
                continue;
            }
            let field = match (&mut self.result, &mut self.arguments) {
                (Some(result), _) if !result.is_empty() => result,
                (_, Some(arguments)) if !arguments.is_empty() => arguments,
                _ => return self,
            };
            // JSON escaping can make a character cost more than its bytes,
            // so cut at least one character per round.
            let mut keep = field.len().saturating_sub(excess.max(1));
            while !field.is_char_boundary(keep) {
                keep -= 1;
            }
            field.truncate(keep);
        }
    }
}

impl ToolDisplayLink {
    pub fn new(uri: impl Into<String>) -> Self {
        Self {
            uri: uri.into(),
            title: None,
            description: None,
        }
    }

    #[must_use]
    pub fn with_title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    #[must_use]
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }
}

fn encoded_len(display: &ToolDisplay) -> usize {
    serde_json::to_vec(display).map_or(usize::MAX, |encoded| encoded.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A display over its cap is cut to it, links first, at a character
    /// boundary, and records the cut; one within it is untouched.
    #[test]
    fn a_display_over_its_cap_is_cut_to_it_and_records_the_cut() {
        let small = ToolDisplay::new()
            .with_arguments("q")
            .with_link(ToolDisplayLink::new("https://a.example"));
        assert_eq!(small.clone().bounded(TOOL_DISPLAY_LIMIT_BYTES), small);

        let large = ToolDisplay::new()
            .with_arguments("é".repeat(400))
            .with_result("ü".repeat(4_000))
            .with_link(ToolDisplayLink::new("https://a.example").with_title("a"))
            .with_link(ToolDisplayLink::new("https://b.example").with_title("b"));
        let original = encoded_len(&large);
        let bounded = large.bounded(1024);
        assert!(encoded_len(&bounded) <= 1024, "{}", encoded_len(&bounded));
        assert!(
            bounded.links.is_empty(),
            "links are dropped before text is cut"
        );
        assert_eq!(
            bounded.truncated,
            Some(ToolDisplayTruncation {
                original_bytes: original as u64,
                limit_bytes: 1024,
            })
        );
        assert_eq!(bounded.arguments.as_deref(), Some("é".repeat(400).as_str()));
        assert!(
            bounded
                .result
                .as_deref()
                .is_some_and(|result| result.starts_with('ü'))
        );
    }
}
