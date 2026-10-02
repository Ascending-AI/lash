//! L3 (FIG-4859): live extension data is a per-materialization input, not a
//! recorded one. Changing a plugin's extension contribution after a
//! session's creation must leave the session's recorded surfaces — its
//! plugin state, its admitted config and the fork initialization a child
//! would replay — byte-identical across reopen, while the reopened session
//! sees the live data as its own.

use super::*;

use lash_core::facade_support::PluginHost;
use lash_core::plugin::{
    PluginDeclaration, PluginExtensionContribution, PluginSessionRequest, SessionAuthorityContext,
};

const EXTENSION: &str = "probe.data";

fn probe(payload: serde_json::Value) -> Arc<dyn PluginFactory> {
    Arc::new(StaticPluginFactory::new(
        PluginDeclaration::initial("extension-probe"),
        lash_core::plugin::PluginSpec::new().with_extension_contribution(
            PluginExtensionContribution::new(EXTENSION, payload).expect("a JSON payload"),
        ),
    ))
}

fn host(payload: serde_json::Value) -> PluginHost {
    PluginHost::new(vec![
        Arc::new(lash_protocol_standard::StandardProtocolPluginFactory::new()),
        probe(payload),
    ])
}

/// The plugin configuration a real creation records: every registered owner
/// resolves its namespace, the protocol's recorded `behaviour` included.
fn creation_authority(host: &PluginHost) -> SessionAuthorityContext {
    let config = host
        .resolve_creation_plugin_config(
            Some(lash_protocol_standard::STANDARD_PROTOCOL_PLUGIN_ID),
            &lash_core::PluginOptions::default(),
            None,
            true,
            &lash_core::store::plugin_writers::PluginAdmission::default(),
        )
        .expect("the creation config resolves");
    SessionAuthorityContext {
        plugin_config: lash_core::AdmittedPluginConfig::new(config, 0),
        ..Default::default()
    }
}

#[test]
fn live_extension_data_changed_after_creation_does_not_rewrite_the_record() -> Result<()> {
    // The session was created while the plugin contributed `{"v": 1}`.
    let created_host = host(serde_json::json!({"v": 1}));
    let created = created_host
        .build_session(PluginSessionRequest::creation(
            "extensions",
            creation_authority(&created_host),
        ))
        .expect("created session");
    assert_eq!(
        created.session_extensions().payloads(EXTENSION),
        &[serde_json::json!({"v": 1})]
    );
    let snapshot = created.export_state();
    let config = created.admitted_plugin_config();
    let init = created.capture_fork_init().expect("fork initialization");

    // The reopened host's plugin contributes `{"v": 2}`: an extension
    // contribution is collected live, once per materialization.
    let reopened = host(serde_json::json!({"v": 2}))
        .build_session(PluginSessionRequest::rematerialization(
            "extensions",
            &snapshot,
            SessionAuthorityContext {
                plugin_config: config.clone(),
                ..Default::default()
            },
        ))
        .expect("reopened session");
    assert_eq!(
        reopened.session_extensions().payloads(EXTENSION),
        &[serde_json::json!({"v": 2})],
        "the reopened session is served the live contribution"
    );

    // Nothing the session recorded moved: not its state, not its admitted
    // config, and not the initialization a forked child would replay.
    assert_eq!(reopened.export_state(), snapshot);
    assert_eq!(reopened.admitted_plugin_config(), config);
    assert_eq!(
        rmp_serde::to_vec_named(&reopened.capture_fork_init().expect("fork init")).expect("encode"),
        rmp_serde::to_vec_named(&init).expect("encode"),
        "the forked-child record is identical across reopen"
    );
    Ok(())
}
