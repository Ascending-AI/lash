use super::*;

#[test]
fn remote_peer_retains_the_execution_controls_the_session_recorded() {
    for no_progress_budget in [
        lash_core::NoProgressBudget::bounded(2),
        lash_core::NoProgressBudget::bounded(17),
        lash_core::NoProgressBudget::Unbounded,
    ] {
        for charge_safety in [
            lash_core::ChargeSafetyPolicy::RequireGuarantee,
            lash_core::ChargeSafetyPolicy::AcceptDuplicateBilling {
                max_unsafe_retries: 0,
                max_duplicate_cost_tokens: Some(0),
            },
            lash_core::ChargeSafetyPolicy::AcceptDuplicateBilling {
                max_unsafe_retries: 2,
                max_duplicate_cost_tokens: Some(4_096),
            },
            lash_core::ChargeSafetyPolicy::AcceptDuplicateBilling {
                max_unsafe_retries: lash_core::ChargeSafetyPolicy::MAX_UNSAFE_RETRIES,
                max_duplicate_cost_tokens: None,
            },
        ] {
            let mut policy = lash_core::SessionPolicy::new(lash_core::TurnBudget::bounded(9));
            policy.no_progress_budget = no_progress_budget;
            policy.charge_safety = charge_safety.clone();
            let recorded = lash_core::PersistedSessionConfig::from(&policy);
            let recorded: lash_core::PersistedSessionConfig = serde_json::from_slice(
                &serde_json::to_vec(&recorded).expect("encode recorded config"),
            )
            .expect("cold-load recorded config");
            let request = RemotePersistProcessEnvRequest {
                env_spec: lash_core::ProcessExecutionEnvSpec {
                    policy: recorded.session_policy(),
                    plugin_config: Default::default(),
                    render: None,
                }
                .into(),
            };
            let bytes = Envelope::at(&crate::negotiation::test_negotiated(), request)
                .encode_json()
                .expect("encode remote environment");
            let peer =
                Envelope::<RemotePersistProcessEnvRequest>::decode_json(&bytes, REMOTE_PROTOCOL)
                    .expect("peer decodes the environment");
            let peer: lash_core::ProcessExecutionEnvSpec = peer
                .body
                .env_spec
                .try_into()
                .expect("peer resolves the execution policy");
            assert_eq!(
                (peer.policy.no_progress_budget, peer.policy.charge_safety),
                (no_progress_budget, charge_safety),
                "the remote peer must run under both recorded controls"
            );
        }
    }
}
