//! S22's final no-catalog assertion waits for the named contraction receipts.
use lash_upgrade_harness::e2e::{
    case::{ArtifactIdentity, CaseSpec, Channel, StoreKind},
    host::HostKind,
    provider::ProviderKind,
};

pub fn spec(store: StoreKind, artifacts: Vec<ArtifactIdentity>) -> CaseSpec {
    CaseSpec {
        id: "S22".into(),
        rules: vec!["L20".into()],
        host: HostKind::UpgradeNode,
        store,
        channel: Channel::Rlm,
        provider: ProviderKind::Scripted,
        restate_nodes: 1,
        artifacts,
        cuts: Vec::new(),
        expected_terminal: "settled".into(),
        requires: ["FIG-4896", "FIG-4897", "FIG-4898", "FIG-4899", "FIG-4900"]
            .into_iter()
            .map(String::from)
            .collect(),
    }
}
