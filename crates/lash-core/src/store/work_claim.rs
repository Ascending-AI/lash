/// Shared data-layer authority record for every claimed unit of session work.
///
/// `C` carries the class-specific material while the six ownership and fencing
/// fields live here exactly once. Class payloads are flattened so the durable
/// serde representation remains identical to the former per-class records.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct WorkClaim<C> {
    pub session_id: String,
    pub claim_id: String,
    pub owner: crate::LeaseOwnerIdentity,
    pub lease_token: String,
    pub fencing_token: u64,
    /// The session-execution-lease generation this claim pins. It controls when
    /// another generation may re-claim the rows, not settlement authority
    /// before that re-claim (ADR 0029).
    pub session_lease_generation: u64,
    #[serde(flatten)]
    pub data: C,
}

impl<C> std::ops::Deref for WorkClaim<C> {
    type Target = C;

    fn deref(&self) -> &Self::Target {
        &self.data
    }
}

impl<C> std::ops::DerefMut for WorkClaim<C> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.data
    }
}

/// Shared settlement receipt for every completed unit of session work.
///
/// `C` carries the class-specific settled row identities while the three claim
/// authority fields live here exactly once. Class payloads are flattened so
/// the durable serde representation remains identical to the former per-class
/// completion records.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WorkCompletion<C> {
    pub session_id: String,
    pub claim_id: String,
    pub lease_token: String,
    #[serde(flatten)]
    pub data: C,
}

impl<C> std::ops::Deref for WorkCompletion<C> {
    type Target = C;

    fn deref(&self) -> &Self::Target {
        &self.data
    }
}

impl<C> std::ops::DerefMut for WorkCompletion<C> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.data
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn authority_fields<C>(data: C) -> WorkClaim<C> {
        WorkClaim {
            session_id: "session".to_string(),
            claim_id: "claim".to_string(),
            owner: crate::LeaseOwnerIdentity::opaque("owner", "incarnation"),
            lease_token: "lease".to_string(),
            fencing_token: 7,
            session_lease_generation: 3,
            data,
        }
    }

    #[test]
    fn queued_claim_json_keeps_the_pre_unification_shape() {
        let claim = authority_fields(crate::QueuedWorkClaimData {
            batches: Vec::new(),
        });
        assert_eq!(
            serde_json::to_string(&claim).expect("serialize queued claim"),
            r#"{"session_id":"session","claim_id":"claim","owner":{"owner_id":"owner","incarnation_id":"incarnation"},"lease_token":"lease","fencing_token":7,"session_lease_generation":3,"batches":[]}"#
        );
    }

    #[test]
    fn turn_input_claim_json_keeps_the_pre_unification_shape() {
        let claim = authority_fields(crate::TurnInputClaimData {
            mode: crate::TurnInputClaimMode::NextTurn,
            inputs: Vec::new(),
            applications: Vec::new(),
        });
        assert_eq!(
            serde_json::to_string(&claim).expect("serialize turn-input claim"),
            r#"{"session_id":"session","claim_id":"claim","owner":{"owner_id":"owner","incarnation_id":"incarnation"},"lease_token":"lease","fencing_token":7,"session_lease_generation":3,"mode":{"kind":"next_turn"},"inputs":[]}"#
        );
    }

    #[test]
    fn active_turn_claim_with_applications_keeps_the_pre_unification_shape() {
        let claim = authority_fields(crate::TurnInputClaimData {
            mode: crate::TurnInputClaimMode::ActiveTurn {
                turn_id: crate::TurnId::from("turn"),
                checkpoint: crate::CheckpointKind::BeforeCompletion,
            },
            inputs: Vec::new(),
            applications: vec![crate::TurnInputApplication {
                input_id: "input".to_string(),
                source_key: Some("source".to_string()),
                turn_id: crate::TurnId::from("turn"),
                committed_message_id: "message".to_string(),
                checkpoint: Some(crate::CheckpointKind::BeforeCompletion),
            }],
        });
        assert_eq!(
            serde_json::to_string(&claim).expect("serialize active-turn claim"),
            r#"{"session_id":"session","claim_id":"claim","owner":{"owner_id":"owner","incarnation_id":"incarnation"},"lease_token":"lease","fencing_token":7,"session_lease_generation":3,"mode":{"kind":"active_turn","turn_id":"turn","checkpoint":"before_completion"},"inputs":[],"applications":[{"input_id":"input","source_key":"source","turn_id":"turn","committed_message_id":"message","checkpoint":"before_completion"}]}"#
        );
    }

    #[test]
    fn queued_completion_json_keeps_the_pre_unification_shape() {
        let completion = crate::QueuedWorkCompletion {
            session_id: "session".to_string(),
            claim_id: "claim".to_string(),
            lease_token: "lease".to_string(),
            data: crate::QueuedWorkCompletionData {
                batch_ids: vec!["batch".to_string()],
            },
        };
        assert_eq!(
            serde_json::to_string(&completion).expect("serialize queued completion"),
            r#"{"session_id":"session","claim_id":"claim","lease_token":"lease","batch_ids":["batch"]}"#
        );
    }

    #[test]
    fn turn_input_completion_json_keeps_the_pre_unification_shape() {
        let completion = crate::TurnInputCompletion {
            session_id: "session".to_string(),
            claim_id: "claim".to_string(),
            lease_token: "lease".to_string(),
            data: crate::TurnInputCompletionData {
                input_ids: vec!["input".to_string()],
                applications: vec![crate::TurnInputApplication {
                    input_id: "input".to_string(),
                    source_key: None,
                    turn_id: crate::TurnId::from("turn"),
                    committed_message_id: "message".to_string(),
                    checkpoint: None,
                }],
            },
        };
        assert_eq!(
            serde_json::to_string(&completion).expect("serialize turn-input completion"),
            r#"{"session_id":"session","claim_id":"claim","lease_token":"lease","input_ids":["input"],"applications":[{"input_id":"input","turn_id":"turn","committed_message_id":"message"}]}"#
        );
    }
}
