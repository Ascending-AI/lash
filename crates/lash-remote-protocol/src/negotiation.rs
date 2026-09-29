//! Remote-protocol version negotiation (ADR 0115 §4).
//!
//! Every connection opens with a bootstrap: the client sends
//! [`Negotiation::Hello`] with every version it speaks, the server answers
//! with [`answer`], and the client builds its [`Negotiated`] version from the
//! [`Negotiation::Accept`]. The selected version is the highest one both
//! ranges contain; disjoint ranges answer [`Negotiation::Unsupported`], and
//! nothing runs on that connection.
//!
//! The bootstrap messages carry no `protocol_version`: they precede
//! selection, so their JSON is frozen forever and every build parses every
//! peer's.

pub use lash_sansio::VersionRange;

use crate::RemoteProtocolError;

/// The remote-protocol versions this build speaks.
///
/// One version wide: the build speaks exactly [`crate::REMOTE_PROTOCOL_VERSION`].
/// A release that adds a version widens it and keeps the older version's
/// encoders as down-conversions.
pub const REMOTE_PROTOCOL: VersionRange = VersionRange::exactly(crate::REMOTE_PROTOCOL_VERSION);

/// One bootstrap message. JSON is tagged by `negotiation` and frozen.
#[derive(
    Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(tag = "negotiation", rename_all = "snake_case")]
pub enum Negotiation {
    /// The client's opening: every version it speaks.
    Hello { supported: VersionRange },
    /// The server's agreement: every version it speaks, and the one it
    /// selected from both ranges.
    Accept {
        supported: VersionRange,
        selected: u32,
    },
    /// The ranges are disjoint, or the connection did not open with a
    /// `Hello`. Nothing runs.
    Unsupported {
        local: VersionRange,
        peer: VersionRange,
    },
}

/// A server's answer to a `Hello`.
///
/// The highest version in both ranges is selected. Anything but a `Hello` is
/// answered `Unsupported` against the range the message carries, because a
/// connection that did not open with a `Hello` runs nothing.
pub fn answer(local: VersionRange, hello: &Negotiation) -> Negotiation {
    let peer = match hello {
        Negotiation::Hello { supported } => {
            return match local.select(*supported) {
                Some(selected) => Negotiation::Accept {
                    supported: local,
                    selected,
                },
                None => Negotiation::Unsupported {
                    local,
                    peer: *supported,
                },
            };
        }
        Negotiation::Accept { supported, .. } => *supported,
        Negotiation::Unsupported { local: peer, .. } => *peer,
    };
    Negotiation::Unsupported { local, peer }
}

/// A connection's selected version. Built only from an `Accept` that this
/// side validated: `selected` is the highest version inside both ranges.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Negotiated {
    selected: u32,
}

impl Negotiated {
    /// Validates the server's answer against the client's `local` range.
    ///
    /// An `Unsupported` answer, a `Hello`, or an `Accept` whose `selected`
    /// is not the highest common version is refused, and the connection runs
    /// nothing.
    pub fn from_accept(
        local: VersionRange,
        accept: &Negotiation,
    ) -> Result<Self, RemoteProtocolError> {
        let refuse = |message: String| RemoteProtocolError::InvalidEnvelope {
            type_name: "Negotiation",
            message,
        };
        match accept {
            Negotiation::Accept {
                supported,
                selected,
            } if local.select(*supported) == Some(*selected) => Ok(Self {
                selected: *selected,
            }),
            Negotiation::Accept {
                supported,
                selected,
            } => Err(refuse(format!(
                "the peer selected remote protocol version {selected}; the highest version \
                 common to this side's {local} and its own {supported} was {:?}",
                local.select(*supported)
            ))),
            Negotiation::Unsupported {
                local: peer_local, ..
            } => Err(RemoteProtocolError::Unsupported {
                local,
                peer: *peer_local,
            }),
            Negotiation::Hello { supported } => Err(refuse(format!(
                "the peer answered a Hello with a Hello ({supported}) instead of an Accept"
            ))),
        }
    }

    /// The version every message on this connection carries.
    pub fn selected(&self) -> u32 {
        self.selected
    }
}

#[cfg(test)]
pub(crate) fn test_negotiated() -> Negotiated {
    let hello = Negotiation::Hello {
        supported: REMOTE_PROTOCOL,
    };
    Negotiated::from_accept(REMOTE_PROTOCOL, &answer(REMOTE_PROTOCOL, &hello))
        .expect("local protocol range accepts itself")
}

#[cfg(test)]
mod tests {
    use super::{Negotiated, Negotiation, VersionRange, answer};

    fn range(min: u32, max: u32) -> VersionRange {
        VersionRange::new(min, max).expect("range")
    }

    #[test]
    fn negotiation_bootstrap_json_is_frozen() {
        let frozen = [
            (
                Negotiation::Hello {
                    supported: range(1, 1),
                },
                r#"{"negotiation":"hello","supported":{"min":1,"max":1}}"#,
            ),
            (
                Negotiation::Accept {
                    supported: range(1, 2),
                    selected: 1,
                },
                r#"{"negotiation":"accept","supported":{"min":1,"max":2},"selected":1}"#,
            ),
            (
                Negotiation::Unsupported {
                    local: range(1, 1),
                    peer: range(2, 2),
                },
                r#"{"negotiation":"unsupported","local":{"min":1,"max":1},"peer":{"min":2,"max":2}}"#,
            ),
        ];
        for (message, json) in frozen {
            assert_eq!(serde_json::to_string(&message).expect("encode"), json);
            assert_eq!(
                serde_json::from_str::<Negotiation>(json).expect("decode"),
                message
            );
        }
        // A range inside a bootstrap message is validated like any other.
        assert!(
            serde_json::from_str::<Negotiation>(
                r#"{"negotiation":"hello","supported":{"min":2,"max":1}}"#
            )
            .is_err()
        );
    }

    #[test]
    fn hello_selects_the_highest_common_version_and_accept_validates() {
        let local = range(1, 1);
        let accept = answer(
            local,
            &Negotiation::Hello {
                supported: range(1, 2),
            },
        );
        assert_eq!(
            accept,
            Negotiation::Accept {
                supported: local,
                selected: 1
            }
        );
        let negotiated = Negotiated::from_accept(range(1, 2), &accept).expect("accepted");
        assert_eq!(negotiated.selected(), 1);

        let refused = answer(
            local,
            &Negotiation::Hello {
                supported: range(2, 2),
            },
        );
        assert_eq!(
            refused,
            Negotiation::Unsupported {
                local,
                peer: range(2, 2)
            }
        );
        assert!(Negotiated::from_accept(range(2, 2), &refused).is_err());

        // An Accept whose selection is outside either range is refused.
        let forged = Negotiation::Accept {
            supported: range(1, 3),
            selected: 3,
        };
        assert!(Negotiated::from_accept(range(1, 2), &forged).is_err());

        // A connection that does not open with a Hello runs nothing.
        assert_eq!(
            answer(local, &forged),
            Negotiation::Unsupported {
                local,
                peer: range(1, 3)
            }
        );
    }
}
