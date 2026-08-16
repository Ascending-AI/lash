use super::*;

#[test]
fn canonical_decode_rejects_first_over_limit_value_depth_for_every_nested_shape() {
    fn decode(value: CanonicalValue) -> Result<Snapshot, SnapshotDecodeError> {
        let wire = CanonicalSnapshot {
            version: LASHLANG_SNAPSHOT_VERSION,
            globals: Some(vec![CanonicalBinding {
                name: "root".to_string(),
                value,
            }]),
            heap: None,
        };
        Snapshot::from_canonical_bytes(
            &rmp_serde::to_vec_named(&wire).expect("hand-crafted canonical wire"),
        )
    }

    let mut record = CanonicalValue::Null {};
    let mut list = CanonicalValue::Null {};
    let mut tuple = CanonicalValue::Null {};
    let mut mixed = CanonicalValue::Null {};
    for level in 0..=MAX_SNAPSHOT_VALUE_DEPTH {
        record = CanonicalValue::Record {
            fields: vec![CanonicalBinding {
                name: "child".to_string(),
                value: record,
            }],
        };
        list = CanonicalValue::List { items: vec![list] };
        tuple = CanonicalValue::Tuple { items: vec![tuple] };
        mixed = match level % 3 {
            0 => CanonicalValue::Record {
                fields: vec![CanonicalBinding {
                    name: "child".to_string(),
                    value: mixed,
                }],
            },
            1 => CanonicalValue::List { items: vec![mixed] },
            _ => CanonicalValue::Tuple { items: vec![mixed] },
        };
    }

    let mut object = CanonicalJsonValue::Null {};
    let mut array = CanonicalJsonValue::Null {};
    for _ in 0..MAX_SNAPSHOT_VALUE_DEPTH {
        object = CanonicalJsonValue::Object {
            fields: vec![CanonicalJsonField {
                name: "child".to_string(),
                value: object,
            }],
        };
        array = CanonicalJsonValue::Array { items: vec![array] };
    }
    let projected = |projection_ref| CanonicalValue::Projected {
        value: CanonicalProjectedValue {
            name: "root".to_string(),
            type_name: "object".to_string(),
            projection_ref: Some(projection_ref),
        },
    };

    for (shape, value) in [
        ("record", record),
        ("list", list),
        ("tuple", tuple),
        ("projected object", projected(object)),
        ("projected array", projected(array)),
        ("mixed", mixed),
    ] {
        assert_eq!(
            decode(value),
            Err(SnapshotDecodeError::ValueDepthLimitExceeded {
                limit: MAX_SNAPSHOT_VALUE_DEPTH,
            }),
            "{shape} must fail at the first over-limit value depth"
        );
    }
}

/// The wire is the third value-entry path, and the only one that can present a
/// record this runtime would never have built. `JSON.parse` and every host
/// result refuse a prototype-chain data key now, so a snapshot carrying one is
/// forged or predates the guard; restoring it would put back exactly the
/// enumerable-but-unreadable key the guard exists to prevent.
#[test]
fn canonical_decode_refuses_a_record_key_naming_the_prototype_chain() {
    for name in ["__proto__", "__defineGetter__", "__lookupSetter__"] {
        let wire = CanonicalSnapshot {
            version: LASHLANG_SNAPSHOT_VERSION,
            globals: Some(vec![CanonicalBinding {
                name: "root".to_string(),
                value: CanonicalValue::Record {
                    fields: vec![CanonicalBinding {
                        name: name.to_string(),
                        value: CanonicalValue::Number { value: 1.0 },
                    }],
                },
            }]),
            heap: None,
        };
        let error = Snapshot::from_canonical_bytes(
            &rmp_serde::to_vec_named(&wire).expect("hand-crafted canonical wire"),
        )
        .expect_err("a prototype-chain record key must not decode");
        assert!(
            error.to_string().contains("names the prototype chain"),
            "{name}: {error}"
        );
    }

    let wire = CanonicalSnapshot {
        version: LASHLANG_SNAPSHOT_VERSION,
        globals: Some(vec![CanonicalBinding {
            name: "root".to_string(),
            value: CanonicalValue::Record {
                fields: vec![CanonicalBinding {
                    name: "__proto".to_string(),
                    value: CanonicalValue::Number { value: 1.0 },
                }],
            },
        }]),
        heap: None,
    };
    Snapshot::from_canonical_bytes(
        &rmp_serde::to_vec_named(&wire).expect("hand-crafted canonical wire"),
    )
    .expect("a name that merely looks similar stays an ordinary data key");
}
