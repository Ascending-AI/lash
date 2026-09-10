use super::*;
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
        current_error.to_string().contains("invalid type"),
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
