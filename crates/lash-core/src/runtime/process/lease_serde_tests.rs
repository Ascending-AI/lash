use super::*;
use serde::Deserialize;
use std::fmt;

fn lease() -> ProcessLease {
    ProcessLease {
        schema_version: PROCESS_LEASE_SCHEMA_VERSION,
        process_id: ProcessId::from("process-lease-wire"),
        owner: crate::LeaseOwnerIdentity::opaque("worker", "worker-incarnation"),
        lease_token: "lease-token".to_string(),
        fencing_token: u64::MAX,
        claimed_at_epoch_ms: u64::MAX - 1,
        expires_at_epoch_ms: u64::MAX - 2,
    }
}

fn assert_lease_eq(actual: &ProcessLease, expected: &ProcessLease) {
    assert_eq!(actual.schema_version, expected.schema_version);
    assert_eq!(actual.process_id, expected.process_id);
    assert_eq!(actual.owner, expected.owner);
    assert_eq!(actual.lease_token, expected.lease_token);
    assert_eq!(actual.fencing_token, expected.fencing_token);
    assert_eq!(actual.claimed_at_epoch_ms, expected.claimed_at_epoch_ms);
    assert_eq!(actual.expires_at_epoch_ms, expected.expires_at_epoch_ms);
}

fn assert_version_error(error: impl fmt::Display, actual: u32) {
    let expected = format!(
        "unsupported process lease schema version {actual}; expected {PROCESS_LEASE_SCHEMA_VERSION}"
    );
    let actual = error.to_string();
    assert!(
        actual.starts_with(&expected),
        "unexpected version error: {actual}"
    );
}

fn messagepack_string(value: &str) -> Vec<u8> {
    assert!(value.len() < 32);
    let mut encoded = vec![0xa0 | value.len() as u8];
    encoded.extend(value.as_bytes());
    encoded
}

fn messagepack_map(entries: Vec<(&str, Vec<u8>)>) -> Vec<u8> {
    messagepack_raw_map(
        entries
            .into_iter()
            .map(|(key, value)| (messagepack_string(key), value))
            .collect(),
    )
}

fn messagepack_raw_map(entries: Vec<(Vec<u8>, Vec<u8>)>) -> Vec<u8> {
    assert!(entries.len() < 16);
    let mut encoded = vec![0x80 | entries.len() as u8];
    for (key, value) in entries {
        encoded.extend(key);
        encoded.extend(value);
    }
    encoded
}

fn messagepack_owner(extra: Option<Vec<u8>>) -> Vec<u8> {
    let mut fields = vec![
        ("owner_id", messagepack_string("worker")),
        ("incarnation_id", messagepack_string("boot")),
    ];
    if let Some(extra) = extra {
        fields.push(("unknown", extra));
    }
    messagepack_map(fields)
}

fn messagepack_duplicate_owner() -> Vec<u8> {
    messagepack_map(vec![
        ("owner_id", messagepack_string("first")),
        ("owner_id", messagepack_string("last")),
        ("incarnation_id", messagepack_string("boot")),
    ])
}

fn messagepack_indexed_owner(zero: Vec<u8>, one: Vec<u8>) -> Vec<u8> {
    messagepack_raw_map(vec![
        (zero, messagepack_string("worker")),
        (one, messagepack_string("boot")),
    ])
}

fn messagepack_lease(
    owner: Vec<u8>,
    lease_token: Vec<u8>,
    schema_version: u8,
    version_first: bool,
    signed_integers: bool,
) -> Vec<u8> {
    let fencing_token = if signed_integers {
        vec![0xd3, 0, 0, 0, 0, 0, 0, 0, 7]
    } else {
        vec![0xcf, 255, 255, 255, 255, 255, 255, 255, 255]
    };
    let mut fields = vec![
        ("process_id", messagepack_string("p")),
        ("owner", owner),
        ("lease_token", lease_token),
        ("fencing_token", fencing_token),
        ("claimed_at_epoch_ms", vec![1]),
        ("expires_at_epoch_ms", vec![2]),
    ];
    fields.insert(
        if version_first { 0 } else { fields.len() },
        ("schema_version", vec![schema_version]),
    );
    messagepack_map(fields)
}

fn assert_messagepack_fixture_decodes(encoded: &[u8], expected_fencing_token: u64) {
    let decoded: ProcessLease =
        rmp_serde::from_slice(encoded).expect("deserialize compatible MessagePack lease map");
    assert_eq!(decoded.schema_version, PROCESS_LEASE_SCHEMA_VERSION);
    assert_eq!(decoded.process_id, ProcessId::from("p"));
    assert_eq!(
        decoded.owner,
        crate::LeaseOwnerIdentity::opaque("worker", "boot")
    );
    assert_eq!(decoded.lease_token, "tok");
    assert_eq!(decoded.fencing_token, expected_fencing_token);
    assert_eq!(decoded.claimed_at_epoch_ms, 1);
    assert_eq!(decoded.expires_at_epoch_ms, 2);
}

fn serde_content_string(value: &str) -> serde_content::Value<'static> {
    serde_content::Value::String(std::borrow::Cow::Owned(value.to_owned()))
}

fn serde_content_lease(
    unknown_owner_value: serde_content::Value<'static>,
    version_first: bool,
    schema_version: u32,
) -> serde_content::Value<'static> {
    let owner = serde_content::Value::Map(vec![
        (
            serde_content_string("owner_id"),
            serde_content_string("worker"),
        ),
        (
            serde_content_string("incarnation_id"),
            serde_content_string("boot"),
        ),
        (serde_content_string("unknown"), unknown_owner_value),
    ]);
    let mut fields = vec![
        (
            serde_content_string("process_id"),
            serde_content_string("p"),
        ),
        (serde_content_string("owner"), owner),
        (
            serde_content_string("lease_token"),
            serde_content_string("tok"),
        ),
        (
            serde_content_string("fencing_token"),
            serde_content::Value::from(u64::MAX),
        ),
        (
            serde_content_string("claimed_at_epoch_ms"),
            serde_content::Value::from(1_u64),
        ),
        (
            serde_content_string("expires_at_epoch_ms"),
            serde_content::Value::from(2_u64),
        ),
    ];
    fields.insert(
        if version_first { 0 } else { fields.len() },
        (
            serde_content_string("schema_version"),
            serde_content::Value::from(schema_version),
        ),
    );
    serde_content::Value::Map(fields)
}

#[test]
fn process_lease_schema_version_helper_reports_actual_and_expected() {
    assert_eq!(
        ensure_process_lease_schema_version(PROCESS_LEASE_SCHEMA_VERSION),
        Ok(())
    );
    assert_eq!(
        ensure_process_lease_schema_version(PROCESS_LEASE_SCHEMA_VERSION + 1),
        Err(ProcessLeaseSchemaVersionError {
            actual: PROCESS_LEASE_SCHEMA_VERSION + 1,
            expected: PROCESS_LEASE_SCHEMA_VERSION,
        })
    );
}

#[test]
fn process_lease_json_output_and_direct_round_trip_stay_stable() {
    let expected = lease();
    let encoded = serde_json::to_string(&expected).expect("serialize process lease");
    assert_eq!(
        encoded,
        r#"{"schema_version":2,"process_id":"process-lease-wire","owner":{"owner_id":"worker","incarnation_id":"worker-incarnation"},"lease_token":"lease-token","fencing_token":18446744073709551615,"claimed_at_epoch_ms":18446744073709551614,"expires_at_epoch_ms":18446744073709551613}"#
    );

    let decoded: ProcessLease =
        serde_json::from_str(&encoded).expect("deserialize current process lease");
    assert_lease_eq(&decoded, &expected);
}

#[test]
fn process_lease_json_nested_round_trips_and_routes_the_version_fence() {
    let expected = lease();
    for outcome in [
        ProcessLeaseClaimOutcome::Acquired(lease()),
        ProcessLeaseClaimOutcome::Busy { holder: lease() },
    ] {
        let encoded = serde_json::to_vec(&outcome).expect("serialize nested lease");
        let decoded: ProcessLeaseClaimOutcome =
            serde_json::from_slice(&encoded).expect("deserialize nested current lease");
        let actual = match decoded {
            ProcessLeaseClaimOutcome::Acquired(lease)
            | ProcessLeaseClaimOutcome::Busy { holder: lease } => lease,
        };
        assert_lease_eq(&actual, &expected);
    }

    let future = PROCESS_LEASE_SCHEMA_VERSION + 1;
    let acquired = format!(r#"{{"Acquired":{{"schema_version":{future}}}}}"#);
    let busy = format!(r#"{{"Busy":{{"holder":{{"schema_version":{future}}}}}}}"#);
    assert_version_error(
        serde_json::from_str::<ProcessLeaseClaimOutcome>(&acquired)
            .expect_err("future acquired lease must be refused"),
        future,
    );
    assert_version_error(
        serde_json::from_str::<ProcessLeaseClaimOutcome>(&busy)
            .expect_err("future busy holder must be refused"),
        future,
    );
}

#[test]
fn process_lease_json_refuses_old_and_future_versions_before_payload_shape() {
    let old = PROCESS_LEASE_SCHEMA_VERSION - 1;
    assert_version_error(
        serde_json::from_str::<ProcessLease>(&format!(r#"{{"schema_version":{old}}}"#))
            .expect_err("old lease must be refused"),
        old,
    );

    let future = PROCESS_LEASE_SCHEMA_VERSION + 1;
    assert_version_error(
        serde_json::from_str::<ProcessLease>(&format!(
            r#"{{"process_id":false,"owner":[],"schema_version":{future}}}"#
        ))
        .expect_err("future lease must be refused before malformed payload"),
        future,
    );

    let current_error = serde_json::from_str::<ProcessLease>(&format!(
        r#"{{"process_id":false,"schema_version":{PROCESS_LEASE_SCHEMA_VERSION}}}"#
    ))
    .expect_err("malformed current lease must fail its payload shape");
    assert!(
        current_error
            .to_string()
            .contains("expected a string, found false"),
        "unexpected malformed-current error: {current_error}"
    );
}

#[test]
fn process_lease_json_version_field_diagnostics_are_deliberate() {
    let missing = serde_json::from_str::<ProcessLease>(r#"{"process_id":false}"#)
        .expect_err("missing version must be refused");
    assert!(
        missing
            .to_string()
            .contains("missing field `schema_version`"),
        "unexpected missing-version error: {missing}"
    );

    let invalid = serde_json::from_str::<ProcessLease>(r#"{"schema_version":"2"}"#)
        .expect_err("invalid version must be refused");
    assert!(
        invalid.to_string().contains("invalid type"),
        "unexpected invalid-version error: {invalid}"
    );

    let duplicate =
        serde_json::from_str::<ProcessLease>(r#"{"schema_version":2,"schema_version":2}"#)
            .expect_err("duplicate version must be refused");
    assert!(
        duplicate
            .to_string()
            .contains("duplicate field `schema_version`"),
        "unexpected duplicate-version error: {duplicate}"
    );
}

#[test]
fn process_lease_json_keeps_ignoring_unknown_fields() {
    let expected = lease();
    let mut value = serde_json::to_value(&expected).expect("serialize process lease");
    value
        .as_object_mut()
        .expect("process lease JSON object")
        .insert(
            "future_hint".to_string(),
            serde_json::json!({"nested": true}),
        );
    let decoded: ProcessLease =
        serde_json::from_value(value).expect("unknown fields remain accepted");
    assert_lease_eq(&decoded, &expected);
}

#[test]
fn process_lease_named_and_positional_messagepack_round_trip_exact_u64_values() {
    let expected = lease();
    for encoded in [
        rmp_serde::to_vec_named(&expected).expect("serialize named MessagePack lease"),
        rmp_serde::to_vec(&expected).expect("serialize positional MessagePack lease"),
    ] {
        let decoded: ProcessLease =
            rmp_serde::from_slice(&encoded).expect("deserialize current MessagePack lease");
        assert_lease_eq(&decoded, &expected);
    }
}

#[test]
fn process_lease_positional_messagepack_refuses_future_version_before_payload() {
    let future = PROCESS_LEASE_SCHEMA_VERSION + 1;
    let encoded = rmp_serde::to_vec(&(future, false)).expect("serialize malformed future lease");
    assert_version_error(
        rmp_serde::from_slice::<ProcessLease>(&encoded)
            .expect_err("future positional lease must be refused before payload"),
        future,
    );
}

#[test]
fn process_lease_messagepack_map_compatibility_is_independent_of_version_field_order() {
    for version_first in [true, false] {
        let fixtures = [
            (
                messagepack_lease(
                    messagepack_owner(None),
                    vec![0xc4, 3, b't', b'o', b'k'],
                    2,
                    version_first,
                    false,
                ),
                u64::MAX,
            ),
            (
                messagepack_lease(
                    messagepack_owner(Some(vec![0xc4, 2, 0xff, 0xfe])),
                    messagepack_string("tok"),
                    2,
                    version_first,
                    false,
                ),
                u64::MAX,
            ),
            (
                messagepack_lease(
                    messagepack_owner(Some(vec![0x81, 0x01, 0x02])),
                    messagepack_string("tok"),
                    2,
                    version_first,
                    false,
                ),
                u64::MAX,
            ),
            (
                messagepack_lease(
                    vec![
                        0x92, 0xa6, b'w', b'o', b'r', b'k', b'e', b'r', 0xa4, b'b', b'o', b'o',
                        b't',
                    ],
                    messagepack_string("tok"),
                    2,
                    version_first,
                    false,
                ),
                u64::MAX,
            ),
            (
                messagepack_lease(
                    messagepack_owner(None),
                    messagepack_string("tok"),
                    2,
                    version_first,
                    true,
                ),
                7,
            ),
        ];

        for (encoded, expected_fencing_token) in fixtures {
            assert_messagepack_fixture_decodes(&encoded, expected_fencing_token);
        }
    }
}

#[test]
fn process_lease_nested_messagepack_maps_share_order_independent_compatibility() {
    for version_first in [true, false] {
        let lease = messagepack_lease(
            messagepack_owner(Some(vec![0xc4, 2, 0xff, 0xfe])),
            vec![0xc4, 3, b't', b'o', b'k'],
            2,
            version_first,
            false,
        );
        for encoded in [
            messagepack_map(vec![("Acquired", lease.clone())]),
            messagepack_map(vec![(
                "Busy",
                messagepack_map(vec![("holder", lease.clone())]),
            )]),
        ] {
            let decoded: ProcessLeaseClaimOutcome = rmp_serde::from_slice(&encoded)
                .expect("nested lease must retain MessagePack compatibility");
            let decoded = match decoded {
                ProcessLeaseClaimOutcome::Acquired(lease)
                | ProcessLeaseClaimOutcome::Busy { holder: lease } => lease,
            };
            assert_eq!(
                decoded.owner,
                crate::LeaseOwnerIdentity::opaque("worker", "boot")
            );
            assert_eq!(decoded.lease_token, "tok");
        }
    }
}

#[test]
fn process_lease_messagepack_owner_numeric_identifiers_match_derived_serde() {
    let unsigned_indexes = [
        (vec![0], vec![1]),
        (vec![0xcc, 0], vec![0xcc, 1]),
        (vec![0xcd, 0, 0], vec![0xcd, 0, 1]),
        (vec![0xce, 0, 0, 0, 0], vec![0xce, 0, 0, 0, 1]),
        (
            vec![0xcf, 0, 0, 0, 0, 0, 0, 0, 0],
            vec![0xcf, 0, 0, 0, 0, 0, 0, 0, 1],
        ),
    ];

    for (zero, one) in unsigned_indexes {
        for version_first in [true, false] {
            let lease = messagepack_lease(
                messagepack_indexed_owner(zero.clone(), one.clone()),
                messagepack_string("tok"),
                PROCESS_LEASE_SCHEMA_VERSION as u8,
                version_first,
                false,
            );
            assert_messagepack_fixture_decodes(&lease, u64::MAX);

            for encoded in [
                messagepack_map(vec![("Acquired", lease.clone())]),
                messagepack_map(vec![(
                    "Busy",
                    messagepack_map(vec![("holder", lease.clone())]),
                )]),
            ] {
                let decoded: ProcessLeaseClaimOutcome = rmp_serde::from_slice(&encoded)
                    .expect("nested lease must accept numeric owner field identifiers");
                let decoded = match decoded {
                    ProcessLeaseClaimOutcome::Acquired(lease)
                    | ProcessLeaseClaimOutcome::Busy { holder: lease } => lease,
                };
                assert_eq!(
                    decoded.owner,
                    crate::LeaseOwnerIdentity::opaque("worker", "boot")
                );
            }
        }
    }
}

#[test]
fn process_lease_messagepack_owner_numeric_identifier_edges_match_derived_serde() {
    for version_first in [true, false] {
        let owner_with_unknown_byte_identifier = messagepack_raw_map(vec![
            (messagepack_string("owner_id"), messagepack_string("worker")),
            (vec![0xc4, 1, 0xff], vec![0xc0]),
            (
                messagepack_string("incarnation_id"),
                messagepack_string("boot"),
            ),
        ]);
        let lease = messagepack_lease(
            owner_with_unknown_byte_identifier,
            messagepack_string("tok"),
            PROCESS_LEASE_SCHEMA_VERSION as u8,
            version_first,
            false,
        );
        assert_messagepack_fixture_decodes(&lease, u64::MAX);
        for encoded in [
            messagepack_map(vec![("Acquired", lease.clone())]),
            messagepack_map(vec![(
                "Busy",
                messagepack_map(vec![("holder", lease.clone())]),
            )]),
        ] {
            let decoded: ProcessLeaseClaimOutcome = rmp_serde::from_slice(&encoded)
                .expect("unknown invalid-UTF8 owner key must remain ignored in nested leases");
            let decoded = match decoded {
                ProcessLeaseClaimOutcome::Acquired(lease)
                | ProcessLeaseClaimOutcome::Busy { holder: lease } => lease,
            };
            assert_eq!(
                decoded.owner,
                crate::LeaseOwnerIdentity::opaque("worker", "boot")
            );
        }

        let owner_with_ignored_index = messagepack_raw_map(vec![
            (vec![0], messagepack_string("worker")),
            (vec![2], vec![0xc4, 1, 0xff]),
            (vec![1], messagepack_string("boot")),
        ]);
        let lease = messagepack_lease(
            owner_with_ignored_index,
            messagepack_string("tok"),
            PROCESS_LEASE_SCHEMA_VERSION as u8,
            version_first,
            false,
        );
        assert_messagepack_fixture_decodes(&lease, u64::MAX);

        for duplicate_owner in [
            messagepack_raw_map(vec![
                (vec![0], messagepack_string("first")),
                (messagepack_string("owner_id"), messagepack_string("last")),
                (vec![1], messagepack_string("boot")),
            ]),
            messagepack_raw_map(vec![
                (vec![0], messagepack_string("first")),
                (vec![0], messagepack_string("last")),
                (vec![1], messagepack_string("boot")),
            ]),
            messagepack_raw_map(vec![
                (
                    vec![0xc4, 8, b'o', b'w', b'n', b'e', b'r', b'_', b'i', b'd'],
                    messagepack_string("first"),
                ),
                (messagepack_string("owner_id"), messagepack_string("last")),
                (vec![1], messagepack_string("boot")),
            ]),
        ] {
            let lease = messagepack_lease(
                duplicate_owner,
                messagepack_string("tok"),
                PROCESS_LEASE_SCHEMA_VERSION as u8,
                version_first,
                false,
            );
            let error = rmp_serde::from_slice::<ProcessLease>(&lease)
                .expect_err("duplicate numeric/text owner aliases must be rejected");
            assert!(
                error.to_string().contains("duplicate field"),
                "unexpected duplicate-owner alias error: {error}"
            );
        }

        for invalid_owner in [
            messagepack_raw_map(vec![
                (vec![0xff], vec![0]),
                (vec![0], messagepack_string("worker")),
                (vec![1], messagepack_string("boot")),
            ]),
            messagepack_raw_map(vec![
                (vec![0xd0, 0], messagepack_string("worker")),
                (vec![1], messagepack_string("boot")),
            ]),
        ] {
            let lease = messagepack_lease(
                invalid_owner,
                messagepack_string("tok"),
                PROCESS_LEASE_SCHEMA_VERSION as u8,
                version_first,
                false,
            );
            rmp_serde::from_slice::<ProcessLease>(&lease)
                .expect_err("signed owner field identifiers must remain invalid");
        }
    }

    let future = PROCESS_LEASE_SCHEMA_VERSION + 1;
    for version_first in [true, false] {
        for owner in [
            messagepack_indexed_owner(vec![0], vec![1]),
            messagepack_raw_map(vec![
                (messagepack_string("owner_id"), messagepack_string("worker")),
                (vec![0xc4, 1, 0xff], vec![0xc0]),
                (
                    messagepack_string("incarnation_id"),
                    messagepack_string("boot"),
                ),
            ]),
        ] {
            let lease = messagepack_lease(
                owner,
                messagepack_string("tok"),
                future as u8,
                version_first,
                false,
            );
            assert_version_error(
                rmp_serde::from_slice::<ProcessLease>(&lease)
                    .expect_err("future version must precede owner replay"),
                future,
            );
            for encoded in [
                messagepack_map(vec![("Acquired", lease.clone())]),
                messagepack_map(vec![(
                    "Busy",
                    messagepack_map(vec![("holder", lease.clone())]),
                )]),
            ] {
                assert_version_error(
                    rmp_serde::from_slice::<ProcessLeaseClaimOutcome>(&encoded)
                        .expect_err("nested future version must precede owner replay"),
                    future,
                );
            }
        }
    }
}

#[test]
fn process_lease_duplicate_owner_fields_remain_errors_in_all_map_paths() {
    for version_first in [true, false] {
        let lease = messagepack_lease(
            messagepack_duplicate_owner(),
            messagepack_string("tok"),
            2,
            version_first,
            false,
        );
        let direct_error = rmp_serde::from_slice::<ProcessLease>(&lease)
            .expect_err("duplicate owner identity must be rejected");
        assert!(
            direct_error.to_string().contains("duplicate field"),
            "unexpected duplicate-owner error: {direct_error}"
        );

        for encoded in [
            messagepack_map(vec![("Acquired", lease.clone())]),
            messagepack_map(vec![(
                "Busy",
                messagepack_map(vec![("holder", lease.clone())]),
            )]),
        ] {
            let nested_error = rmp_serde::from_slice::<ProcessLeaseClaimOutcome>(&encoded)
                .expect_err("nested duplicate owner identity must be rejected");
            assert!(
                nested_error.to_string().contains("duplicate field"),
                "unexpected nested duplicate-owner error: {nested_error}"
            );
        }
    }

    for encoded in [
        r#"{"schema_version":2,"owner":{"owner_id":"first","owner_id":"last","incarnation_id":"boot"},"process_id":"p","lease_token":"tok","fencing_token":1,"claimed_at_epoch_ms":1,"expires_at_epoch_ms":2}"#,
        r#"{"owner":{"owner_id":"first","owner_id":"last","incarnation_id":"boot"},"schema_version":2,"process_id":"p","lease_token":"tok","fencing_token":1,"claimed_at_epoch_ms":1,"expires_at_epoch_ms":2}"#,
    ] {
        let error = serde_json::from_str::<ProcessLease>(encoded)
            .expect_err("JSON duplicate owner identity must be rejected");
        assert!(
            error.to_string().contains("duplicate field"),
            "unexpected JSON duplicate-owner error: {error}"
        );
    }
}

#[test]
fn process_lease_future_version_precedes_buffered_binary_payload_errors() {
    let future = PROCESS_LEASE_SCHEMA_VERSION + 1;
    let encoded = messagepack_lease(
        messagepack_owner(Some(vec![0xc4, 2, 0xff, 0xfe])),
        messagepack_string("tok"),
        future as u8,
        false,
        false,
    );
    assert_version_error(
        rmp_serde::from_slice::<ProcessLease>(&encoded)
            .expect_err("future version must be refused before buffered binary payload"),
        future,
    );

    for encoded in [
        messagepack_map(vec![("Acquired", encoded.clone())]),
        messagepack_map(vec![(
            "Busy",
            messagepack_map(vec![("holder", encoded.clone())]),
        )]),
    ] {
        assert_version_error(
            rmp_serde::from_slice::<ProcessLeaseClaimOutcome>(&encoded)
                .expect_err("nested future version must precede buffered binary payload"),
            future,
        );
    }
}

#[test]
fn process_lease_generic_serde_values_preserve_wide_integers_and_newtypes() {
    let wide_integer = serde_content::Value::from(u128::MAX);
    let newtype = serde_content::Value::Struct(Box::new(serde_content::Struct {
        name: std::borrow::Cow::Borrowed("OpaqueUnknown"),
        data: serde_content::Data::NewType {
            value: serde_content::Value::from(u128::MAX),
        },
    }));

    for version_first in [true, false] {
        for unknown in [wide_integer.clone(), newtype.clone()] {
            let decoded = ProcessLease::deserialize(serde_content::Deserializer::new(
                serde_content_lease(unknown, version_first, PROCESS_LEASE_SCHEMA_VERSION),
            ))
            .expect("generic Serde owner value must not narrow current lease maps");
            assert_eq!(
                decoded.owner,
                crate::LeaseOwnerIdentity::opaque("worker", "boot")
            );
            assert_eq!(decoded.fencing_token, u64::MAX);
        }
    }

    for unknown in [wide_integer, newtype] {
        assert_version_error(
            ProcessLease::deserialize(serde_content::Deserializer::new(serde_content_lease(
                unknown,
                false,
                PROCESS_LEASE_SCHEMA_VERSION + 1,
            )))
            .expect_err("future version must precede generic buffered owner values"),
            PROCESS_LEASE_SCHEMA_VERSION + 1,
        );
    }
}
