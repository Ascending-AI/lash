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
