use super::*;

/// The supported range is two-sided (FIG-3797): for the 1.0 cut it is the
/// single current version, so every other stamp is refused, but the admission
/// predicate and its refusal are range-shaped so a compatibility release can
/// widen the floor without touching the gate again.
#[test]
fn supported_version_is_the_two_sided_range() {
    assert_eq!(
        MIN_SUPPORTED_SCHEMA_VERSION, SCHEMA_VERSION,
        "the 1.0 supported range is the single current component version"
    );
    assert!(supported_version(Some(SCHEMA_VERSION)));
    assert!(!supported_version(Some(SCHEMA_VERSION - 1)));
    assert!(!supported_version(Some(SCHEMA_VERSION + 1)));
    assert!(!supported_version(None));
}

/// The supported-range refusal is typed: the found stamp and both range ends
/// ride as fields so a caller can classify without parsing text, while the
/// rendered message still names them for the operator.
#[test]
fn version_refusal_is_typed_and_names_found_and_range() {
    let error = version_mismatch_error(Some("public"), Some(SCHEMA_VERSION - 1), None);
    let StoreError::SchemaVersionOutOfRange {
        component,
        found,
        supported_min,
        supported_latest,
        message,
    } = &error
    else {
        panic!("the version refusal must be the typed range error: {error:?}")
    };
    assert_eq!(component, SCHEMA_COMPONENT);
    assert_eq!(*found, Some(SCHEMA_VERSION - 1));
    assert_eq!(*supported_min, MIN_SUPPORTED_SCHEMA_VERSION);
    assert_eq!(*supported_latest, SCHEMA_VERSION);
    assert!(
        message.contains(&format!("has version {}", SCHEMA_VERSION - 1))
            && message.contains(&format!(
                "{MIN_SUPPORTED_SCHEMA_VERSION}..={SCHEMA_VERSION}"
            )),
        "the refusal must name the found version and the supported range: {message}"
    );
}

/// The refusal text is derived, not frozen: it must name the direction of the
/// mismatch and the supported range, and must never carry the retired doc-site
/// pointer or a hard-coded historical cutover paragraph (FIG-3172, FIG-3173).
#[test]
fn version_mismatch_refusal_derives_direction_and_range() {
    let older = version_mismatch_error(Some("public"), Some(SCHEMA_VERSION - 1), None).to_string();
    let newer = version_mismatch_error(Some("public"), Some(SCHEMA_VERSION + 1), None).to_string();
    let unstamped = version_mismatch_error(Some("public"), None, None).to_string();
    let uninstalled = version_mismatch_error(None, None, None).to_string();

    assert!(
        older.contains("older or skipped release"),
        "an older stamp must be named as such: {older}"
    );
    assert!(
        older.contains(&format!("expected {SCHEMA_VERSION}")),
        "an older stamp must name the expected version: {older}"
    );
    assert!(
        newer.contains("provisioned by a newer build")
            && newer.contains("never migrates a schema backwards"),
        "a newer stamp must be refused as a downgrade, not as a missing migration: {newer}"
    );
    assert!(
        unstamped.contains("no version stamp") && unstamped.contains("lash_schema_versions"),
        "an unstamped database must be told which row is missing: {unstamped}"
    );
    assert!(
        uninstalled.contains("unprovisioned"),
        "an empty database must be told it is unprovisioned: {uninstalled}"
    );

    for message in [&older, &newer, &unstamped] {
        assert!(
            message.contains("has no applicable migration"),
            "the version-bump companion classifies this refusal by that phrase: {message}"
        );
        assert!(
            !message.contains("persistence.html"),
            "the retired doc site must not be offered as a remedy: {message}"
        );
    }
    // Recreation is the remedy only when this build is the one that should own
    // the database. A newer build's catalog is fixed by deploying the right
    // build, not by dropping it, so that arm names the deploy path instead.
    for message in [&older, &unstamped] {
        assert!(
            message.contains("crates/lash-postgres-store/schema.sql")
                && message.contains("DROP SCHEMA")
                && message.contains("0081-destructive-schema-changes"),
            "the remedy must state the recreate procedure inline: {message}"
        );
    }
    assert!(
        newer.contains("Deploy a build"),
        "a newer catalog must be remedied by deployment, not recreation: {newer}"
    );
    for message in [&older, &newer, &unstamped, &uninstalled] {
        assert!(
            message.contains("supported range") && message.contains("does not relax it"),
            "every arm must name the supported range and the valve's limit: {message}"
        );
    }
}
