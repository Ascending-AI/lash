use super::*;
/// LO-EVICT-PLUGIN-RECORD: a retained checkpoint clone shares its admitted
/// namespace metadata; adopting a prepared commit allocates nothing.
#[test]
fn plugin_record_clone_and_adoption_share_prepared_metadata() {
    let namespace = crate::plugin_state::PluginNamespaceState {
        values: std::collections::BTreeMap::from([("key".into(), serde_json::json!(1))]).into(),
        ..Default::default()
    };
    let body = namespace.body().unwrap();
    let mut record = PluginStateRecord::default();
    record
        .map
        .plugins
        .insert(
            "plugin".repeat(100),
            namespace.entry(body.values()).unwrap(),
        )
        .unwrap();
    record.bodies = Some(Default::default());
    record.insert_body("plugin".repeat(100), body).unwrap();
    record.prepare_adoption().unwrap();
    record.adopt_bodies();
    let measurement = crate::history_growth_law::allocation_counter::Measurement::start();
    let mut copy = record.clone();
    copy.adopt_bodies();
    let counts = measurement.finish();
    assert_eq!(record.map, copy.map);
    assert_eq!(
        counts.allocated, 0,
        "checkpoint metadata copied or grew after preparation: {counts:?}"
    );
}
