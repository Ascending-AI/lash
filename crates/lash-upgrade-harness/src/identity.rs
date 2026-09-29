//! The labels the two node binaries put in their ready files and turn replies.

use serde::{Deserialize, Serialize};

/// Which of the two Phase A builds this binary is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BuildLabel {
    /// The default build: head as it ships.
    #[serde(rename = "n")]
    N,
    /// Head with the `synthetic-next` feature: a synthetic next release.
    #[serde(rename = "n+1")]
    Next,
}

impl BuildLabel {
    /// The build this binary was compiled as.
    pub const fn current() -> Self {
        if cfg!(feature = "synthetic-next") {
            Self::Next
        } else {
            Self::N
        }
    }

    /// The label's spelling in reports and in the scripted provider's reply.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::N => "n",
            Self::Next => "n+1",
        }
    }
}

impl std::fmt::Display for BuildLabel {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::BuildLabel;

    #[test]
    fn build_labels_have_frozen_spellings() {
        assert_eq!(serde_json::to_string(&BuildLabel::N).expect("n"), "\"n\"");
        assert_eq!(
            serde_json::to_string(&BuildLabel::Next).expect("n+1"),
            "\"n+1\""
        );
    }
}
