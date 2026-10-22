use super::*;

use crate::execution_state::retention_tests::Ledger;
use lash_protocol_types::allocation::{AllocationBudget, AllocationLedger};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

/// LO-EVICT-NAMESPACE-EDIT: a refused copy publishes no value and allocates
/// no payload; escaped immutable views keep the original source admission.
#[test]
fn namespace_mutation_admission_survives_readers_and_refuses_growth_atomically() {
    let used = Arc::new(AtomicU64::new(0));
    let ledger = Ledger {
        used: used.clone(),
        limit: 1 << 22,
    };
    let budget = AllocationBudget::new(ledger.clone());
    let source: NamespaceValues =
        BTreeMap::from([("key".into(), Value::String("x".repeat(4096)))]).into();
    let values = source.clone_in(&budget).unwrap();
    let charged = used.load(Ordering::SeqCst);
    assert!(charged >= 4096);
    let mut namespace = PluginNamespaceState {
        values,
        ..Default::default()
    };
    let reader = namespace.values.clone();
    let changes = [crate::tool_run::ResolvedStateChange::Put {
        key: "new".into(),
        value: Value::String("y".repeat(4096)),
    }];
    let blocker = ledger.reserve(ledger.limit - charged).unwrap();
    let measurement = crate::history_growth_law::allocation_counter::Measurement::start();
    let refusal = namespace.apply_changes(&changes);
    let counts = measurement.finish();
    assert!(
        refusal.is_err(),
        "a full owner must refuse before cloning the values"
    );
    assert!(
        counts.allocated < 256,
        "refusal copied namespace payload: {counts:?}"
    );
    assert!(namespace.values.ptr_eq(&reader));
    assert!(reader.get("new").is_none());
    drop(blocker);
    namespace.apply_changes(&changes).unwrap();
    assert!(namespace.values.contains_key("new"));
    assert!(reader.get("new").is_none());
    assert!(used.load(Ordering::SeqCst) > charged);
    drop(namespace);
    assert_eq!(used.load(Ordering::SeqCst), charged);
    assert_eq!(reader["key"], Value::String("x".repeat(4096)));
    drop(reader);
    assert_eq!(used.load(Ordering::SeqCst), 0);
}

/// LO-EVICT-PLUGIN-MAPS: snapshots share the admitted namespace map,
/// including names and publication metadata.
#[test]
fn plugin_state_and_checkpoint_map_clones_share_their_sources() {
    let plugins = BTreeMap::from([("plugin".repeat(100), PluginNamespaceState::default())]);
    let state = PluginState {
        plugins: plugins.into(),
    };
    let map = PluginStateMap {
        plugins: BTreeMap::from([(
            "plugin".repeat(100),
            state
                .plugins
                .values()
                .next()
                .unwrap()
                .entry(&crate::BlobRef("a".repeat(64)))
                .unwrap(),
        )])
        .into(),
    };
    let measurement = crate::history_growth_law::allocation_counter::Measurement::start();
    let state_copy = state.clone();
    let map_copy = map.clone();
    let counts = measurement.finish();
    assert_eq!(state_copy, state);
    assert_eq!(map_copy, map);
    assert_eq!(
        counts.allocated, 0,
        "immutable namespace maps copied outside admission: {counts:?}"
    );
}

/// LO-EVICT-NAMESPACE-SNAPSHOT: cloning a resident graph must share its
/// admitted maps, so a retained publication never allocates behind the cap.
#[test]
fn namespace_body_clone_shares_retained_maps() {
    let values = BTreeMap::from([("key".into(), Value::String("x".repeat(4096)))]);
    let body = NamespaceBody::encode(
        &values,
        &lash_protocol_types::allocation::AllocationBudget::unowned(),
    )
    .unwrap();
    let measurement = crate::history_growth_law::allocation_counter::Measurement::start();
    let snapshot = body.clone();
    let counts = measurement.finish();
    assert_eq!(body, snapshot);
    assert_eq!(
        counts.allocated, 0,
        "snapshot copied retained graph maps: {counts:?}"
    );
}

/// LO-EVICT-NAMESPACE-GRAPH: cache preparation and publication views
/// admit their maps, trie paths and buffers before materializing them.
#[test]
fn namespace_graph_and_publication_refuse_before_materialization() {
    let used = Arc::new(AtomicU64::new(0));
    let ledger = Ledger {
        used: used.clone(),
        limit: 1 << 24,
    };
    let budget = AllocationBudget::new(ledger.clone());
    let values = NamespaceValues::copy_from_map_in(
        &BTreeMap::from([("key".into(), Value::String("x".repeat(4096)))]),
        &budget,
    )
    .unwrap();
    let namespace = PluginNamespaceState {
        values,
        ..Default::default()
    };
    let charged = used.load(Ordering::SeqCst);
    let blocker = ledger.reserve(ledger.limit - charged).unwrap();
    let measurement = crate::history_growth_law::allocation_counter::Measurement::start();
    let result = namespace.body();
    let counts = measurement.finish();
    assert!(
        result.is_err(),
        "an exhausted owner encoded an unadmitted graph"
    );
    assert!(
        counts.allocated < 256,
        "refusal materialized graph payload: {counts:?}"
    );
    assert_eq!(used.load(Ordering::SeqCst), ledger.limit);
    drop(blocker);
    let body = namespace.body().unwrap();
    let prepared = used.load(Ordering::SeqCst);
    assert!(prepared > charged);
    let blocker = ledger.reserve(ledger.limit - prepared).unwrap();
    let measurement = crate::history_growth_law::allocation_counter::Measurement::start();
    let result = body.publication_after(None, true);
    let counts = measurement.finish();
    assert!(
        result.is_err(),
        "publication map replacement bypassed its source ledger"
    );
    assert!(
        counts.allocated < 256,
        "refusal copied publication maps: {counts:?}"
    );
    drop(blocker);
    let publication = body.publication_after(None, true).unwrap();
    drop(namespace);
    drop(body);
    assert_eq!(
        publication.snapshot()["key"],
        Value::String("x".repeat(4096))
    );
    assert!(used.load(Ordering::SeqCst) >= prepared);
    drop(publication);
    assert_eq!(used.load(Ordering::SeqCst), 0);
}
