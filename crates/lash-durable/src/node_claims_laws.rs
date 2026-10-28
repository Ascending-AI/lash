use crate::{BootId, DurableInstant, NodeId, NodeLease, Owner};

/// LEASE-ADVERTISEMENT-SHARING: cloning a lease preserves its immutable token storage.
#[test]
fn a_cloned_node_lease_shares_its_immutable_advertisement() {
    let lease = NodeLease {
        owner: Owner {
            node: NodeId::new("lease-sharing"),
            boot: BootId::new("boot"),
        },
        decodes: Vec::new(),
        serves: vec![format!("helper-library:{}", "a".repeat(64))].into(),
        capabilities: Default::default(),
        ttl_millis: 1,
        expires_at: DurableInstant(0),
    };
    let cloned = lease.clone();
    let original: &[String] = lease.serves.as_ref();
    let copied: &[String] = cloned.serves.as_ref();
    assert_eq!(
        original[0].as_ptr(),
        copied[0].as_ptr(),
        "cloning a node lease copied its immutable advertisement",
    );
    assert_eq!(
        serde_json::from_str::<Vec<String>>(lease.serves.as_json()).unwrap(),
        original,
    );
    assert_eq!(
        lease.serves.as_json().as_ptr(),
        cloned.serves.as_json().as_ptr()
    );
    assert!(std::sync::Arc::ptr_eq(
        &lease.capabilities,
        &cloned.capabilities
    ));
}
