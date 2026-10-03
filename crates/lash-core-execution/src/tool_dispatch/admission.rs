//! K1 member and round admission over the admitted manifests.
//!
//! A round's calls are admitted together, before any of them prepares or
//! starts: each call's manifest is the one it is admitted under — the
//! catalog's answer, its grant's, or a replayed cell's recorded binding —
//! and its [`ToolDeclaration`](crate::ToolDeclaration) must be valid and
//! supported. One refused member admits no member: every call of the round
//! answers a typed [`ToolAdmissionRefusal`], so no body, hook or provider
//! preparation runs for any of them.
//!
//! A tool the catalog does not hold, or a call whose arguments or
//! before-hooks refuse it, is not an admission refusal: it settles as that
//! one member's failure, exactly as before.

use crate::{ToolAdmissionRefusal, ToolFailure, ToolFailureCause, ToolFailureClass, ToolManifest};

/// Whether a process implementation is bound to run `manifest`'s tool
/// isolated. None is yet: an isolated declaration refuses at admission rather
/// than ever running its body inline.
fn supports_isolation(_manifest: &ToolManifest) -> bool {
    false
}

/// Admit one call under the manifest it is admitted under.
///
/// # Errors
///
/// An invalid declaration, or an isolated one no process implementation
/// runs.
pub(crate) fn admit_tool(manifest: &ToolManifest) -> Result<(), ToolAdmissionRefusal> {
    manifest.declaration.admit(supports_isolation(manifest))
}

/// The failure a call refused at admission answers with.
#[must_use]
pub fn admission_failure(tool_name: &str, refusal: ToolAdmissionRefusal) -> ToolFailure {
    let class = match refusal {
        ToolAdmissionRefusal::Declaration { .. } => ToolFailureClass::Internal,
        ToolAdmissionRefusal::UnsupportedIsolation | ToolAdmissionRefusal::Sibling { .. } => {
            ToolFailureClass::Unavailable
        }
    };
    ToolFailure::runtime(
        class,
        ToolAdmissionRefusal::CODE,
        format!("tool `{tool_name}` was refused at admission: {refusal}"),
    )
    .with_cause(ToolFailureCause::Admission { refusal })
}

/// A refused round: the first refused member, in source order, and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolRoundRefusal {
    member: u32,
    refusal: ToolAdmissionRefusal,
}

impl ToolRoundRefusal {
    /// The refusal member `index` of the round answers: its own cause for the
    /// refused member, and the refused member's position for every sibling.
    #[must_use]
    pub fn refusal_for(&self, index: usize) -> ToolAdmissionRefusal {
        if u32::try_from(index).is_ok_and(|index| index == self.member) {
            self.refusal.clone()
        } else {
            ToolAdmissionRefusal::Sibling {
                member: self.member,
            }
        }
    }

    /// The failure member `index`, calling `tool_name`, answers with.
    #[must_use]
    pub fn failure_for(&self, index: usize, tool_name: &str) -> ToolFailure {
        admission_failure(tool_name, self.refusal_for(index))
    }
}

/// Admit a round's calls together, before any of them prepares or starts.
/// `manifests` are the members' admitted manifests in source order; `None`
/// is a member the catalog does not hold, which settles as its own failure
/// and refuses nothing else.
///
/// # Errors
///
/// The first refused member, which refuses the whole round.
pub fn admit_tool_round<'a>(
    manifests: impl IntoIterator<Item = Option<&'a ToolManifest>>,
) -> Result<(), ToolRoundRefusal> {
    for (member, manifest) in (0_u32..).zip(manifests) {
        if let Some(manifest) = manifest {
            admit_tool(manifest).map_err(|refusal| ToolRoundRefusal { member, refusal })?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ToolDeclaration, ToolDefinition};

    fn manifest(name: &str, declaration: ToolDeclaration) -> ToolManifest {
        ToolDefinition::new(
            name,
            name,
            "",
            crate::SchemaContract::default(),
            crate::SchemaContract::default(),
        )
        .with_declaration(declaration)
        .manifest()
    }

    #[test]
    fn one_refused_member_refuses_every_member_of_its_round() {
        let plain = manifest("plain", ToolDeclaration::default());
        let isolated = manifest(
            "isolated",
            ToolDeclaration {
                isolated: true,
                ..ToolDeclaration::default()
            },
        );
        let refused = admit_tool_round([Some(&plain), None, Some(&isolated)])
            .expect_err("an unsupported isolated member refuses its round");
        assert_eq!(
            refused.refusal_for(2),
            ToolAdmissionRefusal::UnsupportedIsolation
        );
        assert_eq!(
            refused.refusal_for(0),
            ToolAdmissionRefusal::Sibling { member: 2 }
        );
        assert_eq!(
            refused.refusal_for(1),
            ToolAdmissionRefusal::Sibling { member: 2 }
        );
        let failure = refused.failure_for(0, "plain");
        assert_eq!(failure.code, ToolAdmissionRefusal::CODE);
        assert_eq!(
            failure.cause.as_deref(),
            Some(&ToolFailureCause::Admission {
                refusal: ToolAdmissionRefusal::Sibling { member: 2 }
            })
        );
        admit_tool_round([Some(&plain), None]).expect("a valid round is admitted");
    }

    #[test]
    fn an_invalid_declaration_refuses_before_isolation_is_considered() {
        let invalid = manifest(
            "invalid",
            ToolDeclaration {
                may_defer: true,
                isolated: true,
                ..ToolDeclaration::default()
            },
        );
        assert_eq!(
            admit_tool(&invalid),
            Err(ToolAdmissionRefusal::Declaration {
                cause: crate::DeclarationRefusal::IsolatedInlineCapability
            })
        );
    }
}
