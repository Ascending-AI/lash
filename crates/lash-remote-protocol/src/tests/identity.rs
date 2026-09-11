use lash_sansio::SessionId;

use super::*;

#[test]
fn remote_owner_scope_validation_matches_each_core_owner_grammar() {
    for owner in [
        RemoteTriggerOwnerScope::Session {
            session_id: SessionId::from("owner/session:alpha"),
        },
        RemoteTriggerOwnerScope::Session {
            session_id: SessionId::from("  "),
        },
        RemoteTriggerOwnerScope::Host {
            binding_id: "host binding/alpha:1".to_string(),
        },
        RemoteTriggerOwnerScope::Platform,
    ] {
        owner
            .validate("RemoteTriggerOwnerScope")
            .expect("existing non-empty core owner grammar must survive");
    }

    for owner in [
        RemoteTriggerOwnerScope::Session {
            session_id: SessionId::from(""),
        },
        RemoteTriggerOwnerScope::Host {
            binding_id: String::new(),
        },
        RemoteTriggerOwnerScope::Session {
            session_id: SessionId::from("owner\0session"),
        },
        RemoteTriggerOwnerScope::Host {
            binding_id: "host\0binding".to_string(),
        },
    ] {
        assert!(
            owner.validate("RemoteTriggerOwnerScope").is_err(),
            "invalid owner identifiers must be refused"
        );
    }
}

#[test]
fn remote_cause_validation_preserves_partial_trigger_identity_and_checks_effect_scope() {
    let partial = RemoteCausalRef::TriggerOccurrence {
        occurrence_id: "occurrence:partial".to_string(),
        subscription_id: Some("subscription:known".to_string()),
        subscription_incarnation: None,
        subscription_revision: None,
    };
    partial
        .validate("RemoteCausalRef")
        .expect("truthful partial trigger cause");
    let encoded = serde_json::to_value(&partial).expect("encode partial cause");
    assert_eq!(
        serde_json::from_value::<RemoteCausalRef>(encoded).expect("decode partial cause"),
        partial
    );

    let invalid_effect = RemoteCausalRef::Effect {
        address: lash_sansio::EffectAddress {
            execution_scope: lash_sansio::ExecutionScope::runtime_operation(" "),
            replay_key: "replay".to_string(),
        },
    };
    assert!(invalid_effect.validate("RemoteCausalRef").is_err());
}
