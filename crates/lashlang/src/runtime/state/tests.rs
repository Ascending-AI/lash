    use super::*;

    #[test]
    fn canonical_encoding_is_deterministic_for_map_order_and_nan_payload() {
        let left_nan = f64::from_bits(0x7ff0_0000_0000_0001);
        let right_nan = f64::from_bits(0xfff8_0000_0000_0042);

        let mut left_record = Record::new();
        left_record.insert("z".to_string(), Value::Number(left_nan));
        left_record.insert("a".to_string(), Value::String("same\0\u{fffd}".into()));
        let mut left_globals = Record::new();
        left_globals.insert("z-last".to_string(), Value::Bool(true));
        left_globals.insert("session".to_string(), Value::Record(Arc::new(left_record)));

        let mut right_record = Record::new();
        right_record.insert("a".to_string(), Value::String("same\0\u{fffd}".into()));
        right_record.insert("z".to_string(), Value::Number(right_nan));
        let mut right_globals = Record::new();
        right_globals.insert("session".to_string(), Value::Record(Arc::new(right_record)));
        right_globals.insert("z-last".to_string(), Value::Bool(true));

        let left = Snapshot::new(left_globals)
            .to_canonical_bytes()
            .expect("left encode");
        let right = Snapshot::new(right_globals)
            .to_canonical_bytes()
            .expect("right encode");

        assert_eq!(left, right);
    }

    #[test]
    fn canonical_decode_rejects_non_minimal_integer_width_with_location() {
        let snapshot = Snapshot::new(
            [(
                "root".to_string(),
                Value::Projected(
                    ProjectedValue::unavailable_after_restore_with_projection_ref(
                        "root",
                        "number",
                        Some(serde_json::json!(1)),
                    ),
                ),
            )]
            .into_iter()
            .collect(),
        );
        let mut bytes = snapshot.to_canonical_bytes().expect("canonical bytes");
        let needle = [0xa5, b'v', b'a', b'l', b'u', b'e', 0x01];
        let offset = bytes
            .windows(needle.len())
            .rposition(|window| window == needle)
            .expect("projection JSON integer");
        bytes.splice(
            offset + needle.len() - 1..offset + needle.len(),
            [0xcc, 0x01],
        );

        let error = Snapshot::from_canonical_bytes(&bytes)
            .expect_err("non-minimal integer width must be rejected");
        assert!(
            matches!(
                &error,
                SnapshotDecodeError::NonCanonicalEncoding { location, reason }
                    if location == "snapshot.heap.roots[0].value.value.projection_ref.value"
                        && reason.contains("integer width is not minimal")
            ),
            "{error:?}"
        );
    }

    #[test]
    fn canonical_decode_rejects_integer_encoded_runtime_number() {
        let snapshot = Snapshot::new(
            [("root".to_string(), Value::Number(1.0))]
                .into_iter()
                .collect(),
        );
        let mut bytes = snapshot.to_canonical_bytes().expect("canonical bytes");
        let mut needle = vec![0xa5, b'v', b'a', b'l', b'u', b'e', 0xcb];
        needle.extend_from_slice(&1.0_f64.to_bits().to_be_bytes());
        let offset = bytes
            .windows(needle.len())
            .position(|window| window == needle)
            .expect("runtime f64");
        bytes.splice(offset + 6..offset + needle.len(), [0x01]);

        let error = Snapshot::from_canonical_bytes(&bytes)
            .expect_err("integer-encoded runtime number must be rejected");
        assert!(matches!(
            &error,
            SnapshotDecodeError::NonCanonicalEncoding { location, reason }
                if location == "globals.root.value"
                    && reason.contains("must use f64 encoding")
        ));
    }

    #[test]
    fn canonical_decode_rejects_sequence_form_structs() {
        let wire = CanonicalSnapshot {
            version: LASHLANG_SNAPSHOT_VERSION,
            globals: vec![CanonicalBinding {
                name: "root".to_string(),
                value: CanonicalValue::Null {},
            }],
            heap: CanonicalHeap::default(),
        };
        let bytes = rmp_serde::to_vec(&wire).expect("sequence-form bytes");

        let error = Snapshot::from_canonical_bytes(&bytes)
            .expect_err("sequence-form structs must be rejected");
        assert!(
            matches!(
                &error,
                SnapshotDecodeError::NonCanonicalEncoding { location, reason }
                    if location == "snapshot" && reason.contains("map form, not sequence form")
            ),
            "{error:?}"
        );
    }

    #[test]
    fn canonical_decode_rejects_unsorted_and_duplicate_dynamic_keys() {
        for names in [["z", "a"], ["same", "same"]] {
            let wire = CanonicalSnapshot {
                version: LASHLANG_SNAPSHOT_VERSION,
                globals: names
                    .into_iter()
                    .map(|name| CanonicalBinding {
                        name: name.to_string(),
                        value: CanonicalValue::Null {},
                    })
                    .collect(),
                heap: CanonicalHeap::default(),
            };
            let bytes = rmp_serde::to_vec_named(&wire).expect("non-canonical bytes");

            let error = Snapshot::from_canonical_bytes(&bytes)
                .expect_err("dynamic keys must be sorted and unique");
            assert!(matches!(
                &error,
                SnapshotDecodeError::NonCanonicalEncoding { location, reason }
                    if location == "globals"
                        && reason.contains("strictly sorted and unique")
            ));
        }
    }

    #[test]
    fn canonical_encode_error_names_the_nested_value_path() {
        let mut too_deep = Value::Null;
        for _ in 0..=MAX_SNAPSHOT_VALUE_DEPTH {
            too_deep = Value::List(vec![too_deep].into());
        }
        let mut session = Record::new();
        session.insert(
            "items".to_string(),
            Value::List(vec![Value::Null, Value::Null, Value::Null, too_deep].into()),
        );
        let snapshot = Snapshot::new(
            [("session".to_string(), Value::Record(Arc::new(session)))]
                .into_iter()
                .collect(),
        );

        let error = snapshot
            .to_canonical_bytes()
            .expect_err("over-depth value must fail at encode");
        let ContinuationError::UnserializableValue { location, variant } = error else {
            panic!("expected typed unserializable-value error");
        };
        assert!(
            location.starts_with("globals.session.items[3]"),
            "{location}"
        );
        assert_eq!(variant, "value beyond the snapshot depth limit");
    }

    #[test]
    fn canonical_decode_rejects_a_depth_bomb_before_deserializing() {
        let mut value = CanonicalValue::Null {};
        for _ in 0..120 {
            value = CanonicalValue::List { items: vec![value] };
        }
        let bomb = CanonicalSnapshot {
            version: LASHLANG_SNAPSHOT_VERSION,
            globals: vec![CanonicalBinding {
                name: "bomb".to_string(),
                value,
            }],
            heap: CanonicalHeap::default(),
        };
        let bytes = rmp_serde::to_vec_named(&bomb).expect("construct depth bomb");

        assert_eq!(
            Snapshot::from_canonical_bytes(&bytes),
            Err(SnapshotDecodeError::ValueDepthLimitExceeded {
                limit: MAX_SNAPSHOT_VALUE_DEPTH,
            })
        );
    }

    #[test]
    fn canonical_wire_golden_covers_every_value_kind_and_projection_ref() {
        let image = ImageValue::new(
            "sha256:00ff",
            crate::MediaType::parse("image/png").expect("media type"),
            "pixel",
            2,
            Some(1),
            Some(1),
        );
        let projection_ref = serde_json::json!({
            "array": [null, true, 7, "bytes\u{0000}\u{007f}"],
            "object": {"key": "value"}
        });
        let snapshot = Snapshot::new(
            [
                ("bool".to_string(), Value::Bool(true)),
                ("image".to_string(), Value::Image(Box::new(image))),
                ("list".to_string(), Value::List(vec![Value::Null].into())),
                ("null".to_string(), Value::Null),
                ("number".to_string(), Value::Number(-12.5)),
                (
                    "projected".to_string(),
                    Value::Projected(
                        ProjectedValue::unavailable_after_restore_with_projection_ref(
                            "memory",
                            "object",
                            Some(projection_ref),
                        ),
                    ),
                ),
                (
                    "record".to_string(),
                    Value::Record(Arc::new(
                        [("field".to_string(), Value::String("body".into()))]
                            .into_iter()
                            .collect(),
                    )),
                ),
                (
                    "resource".to_string(),
                    Value::Resource(ResourceHandle::new("files", "workspace")),
                ),
                (
                    "string".to_string(),
                    Value::String("body\u{0000}\u{007f}".into()),
                ),
                (
                    "tuple".to_string(),
                    Value::Tuple(vec![Value::Number(1.0), Value::String("two".into())].into()),
                ),
            ]
            .into_iter()
            .collect(),
        );
        let bytes = snapshot.to_canonical_bytes().expect("golden snapshot");
        use sha2::Digest as _;
        assert_eq!(bytes.len(), 1_931);
        assert_eq!(
            sha2::Sha256::digest(&bytes).as_slice(),
            &[
                0xbf, 0x7f, 0x90, 0xd2, 0x67, 0xe3, 0xa5, 0x2f, 0xbc, 0x5c, 0xb2, 0xab, 0xc7, 0x7e,
                0x91, 0x37, 0xfc, 0xc4, 0xff, 0xac, 0xf4, 0xb4, 0x2c, 0x81, 0x8a, 0xa8, 0x01, 0x6b,
                0x22, 0x6f, 0xee, 0xa8,
            ]
        );
    }

    #[test]
    fn canonical_empty_heap_has_exact_golden_bytes() {
        let bytes = Snapshot::default()
            .to_canonical_bytes()
            .expect("empty canonical snapshot");
        let hex = bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        assert_eq!(
            hex,
            concat!(
                "83a776657273696f6e02a7676c6f62616c7390a46865617086a76e6578745f696401",
                "b2616c6c6f636174696f6e5f636f756e74657200b26c6976655f6c6f676963616c",
                "5f627974657300b573697a655f7363686564756c655f76657273696f6e01a5726f6f",
                "747390a76f626a6563747390"
            )
        );
    }

    #[test]
    fn canonical_decode_accepts_every_max_depth_encode_shape() {
        fn round_trip(value: Value) {
            let snapshot = Snapshot::new([("root".to_string(), value)].into_iter().collect());
            let bytes = snapshot.to_canonical_bytes().expect("max-depth encode");
            let decoded = Snapshot::from_canonical_bytes(&bytes).expect("max-depth decode");
            assert_eq!(decoded, snapshot);
        }

        let mut record = Value::Null;
        for _ in 0..MAX_SNAPSHOT_VALUE_DEPTH {
            record = Value::Record(Arc::new(
                [("child".to_string(), record)].into_iter().collect(),
            ));
        }
        round_trip(record);

        let mut list = Value::Null;
        for _ in 0..MAX_SNAPSHOT_VALUE_DEPTH {
            list = Value::List(vec![list].into());
        }
        round_trip(list);

        let mut projection_ref = serde_json::Value::Null;
        // `Projected` enters its JSON payload at depth one, so 63 nested
        // objects place the terminal null at the shared depth limit of 64.
        for _ in 0..MAX_SNAPSHOT_VALUE_DEPTH - 1 {
            projection_ref = serde_json::json!({"child": projection_ref});
        }
        round_trip(Value::Projected(
            ProjectedValue::unavailable_after_restore_with_projection_ref(
                "root",
                "object",
                Some(projection_ref),
            ),
        ));
    }
