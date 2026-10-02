use super::*;

#[cfg(not(feature = "synthetic-next"))]
#[test]
fn production_catalogs_start_at_the_release_baseline() {
    let expand: Vec<_> = EXPAND_MIGRATIONS
        .iter()
        .map(|step| (step.id, step.from_version, step.to_version))
        .collect();
    let backfill: Vec<_> = BACKFILL_MIGRATIONS
        .iter()
        .map(|step| (step.id, step.after_fleet))
        .collect();
    let contract: Vec<_> = CONTRACT_MIGRATIONS
        .iter()
        .map(|step| (step.id, step.after_fleet))
        .collect();
    assert!(
        expand.is_empty() && backfill.is_empty() && contract.is_empty(),
        "production baseline catalogs must be empty: expand={expand:?}, backfill={backfill:?}, contract={contract:?}"
    );
}

/// The upgrade path and the bootstrap provision identical objects: a
/// catalog step that creates an object states it exactly the way
/// `schema.sql` does, so the two paths can never disagree about that
/// object's shape. A step that alters an existing table carries no byte
/// contract with the artifact — `schema.sql` folds the added column into
/// its `CREATE TABLE` body — so the structural suites own that
/// equivalence.
#[test]
fn created_objects_match_the_schema_artifact() {
    for migration in EXPAND_MIGRATIONS {
        if migration.statements.starts_with("CREATE") {
            assert!(
                SCHEMA_DDL.contains(migration.statements),
                "schema.sql does not contain {}'s DDL verbatim",
                migration.id
            );
        }
    }
}

/// The bootstrap and a host applying `schema.sql` seed the same `F` that
/// [`seed_fleet_format`] seeds on an installed catalog: the migrating
/// build's writable floor (ADR 0115 §2.1).
#[test]
fn the_schema_artifact_seeds_the_migrating_build_s_fleet_epoch() {
    let seed = lash_core_execution::FleetFormat::seed(lash_core_execution::FleetFormat::writable());
    let statement = format!(
        "INSERT INTO lash_fleet_format (singleton, format_version)\nVALUES (TRUE, {seed})\nON CONFLICT (singleton) DO NOTHING;"
    );
    assert!(
        SCHEMA_DDL.contains(&statement),
        "schema.sql must seed lash_fleet_format at F={seed}"
    );
}

/// The catalog must chain to the current component: a step targeting a
/// version the build no longer stamps would leave planning stuck.
#[test]
fn the_expand_catalog_chains_to_the_current_component() {
    let mut at = SCHEMA_VERSION;
    while let Some(migration) = EXPAND_MIGRATIONS
        .iter()
        .find(|migration| migration.to_version == at)
    {
        assert_eq!(
            migration.from_version,
            at - 1,
            "{} does not chain from the previous component",
            migration.id
        );
        at = migration.from_version;
    }
}

/// The 1.0 release carries no production expand step. The first one
/// registered must come with a law that applies it to the previous catalog
/// and calls the tolerant checker, replacing this one.
#[cfg(not(feature = "synthetic-next"))]
#[test]
fn every_expand_step_passes_the_previous_tolerant_check() {
    assert!(
        EXPAND_MIGRATIONS.is_empty(),
        "a post-cut expand needs a previous-catalog tolerant check"
    );
}

#[test]
fn phases_round_trip_through_their_names() {
    for phase in [
        MigrationPhase::Expand,
        MigrationPhase::Backfill,
        MigrationPhase::Contract,
    ] {
        assert_eq!(MigrationPhase::parse(phase.name()), Some(phase));
    }
    assert_eq!(MigrationPhase::parse("sideways"), None);
}

/// Every backfill a contract step waits for is one this build carries,
/// and every step's epoch is one a finalize can reach: a contract that
/// named a backfill no build runs would be refused forever.
#[test]
fn contract_steps_wait_only_for_backfills_this_build_carries() {
    for contract in CONTRACT_MIGRATIONS {
        for backfill in contract.after_backfills {
            let carried = BACKFILL_MIGRATIONS
                .iter()
                .find(|carried| carried.id == *backfill)
                .unwrap_or_else(|| panic!("{} waits for unknown backfill {backfill}", contract.id));
            assert!(
                carried.after_fleet <= contract.after_fleet,
                "{} could contract before {backfill} may run",
                contract.id
            );
        }
        assert!(contract.min_reader >= 1, "{}", contract.id);
    }
    // `F` is 1 at the cut and moves at every compatibility release's
    // finalize, so a backfill released at 1 would run before any.
    for backfill in BACKFILL_MIGRATIONS {
        assert!(
            backfill.after_fleet > 1,
            "{} would run before any finalize",
            backfill.id
        );
    }
}

/// A contract step is refused until `F` reaches its epoch, then until
/// every backfill it names is applied, and each refusal names its remedy.
#[test]
fn a_contract_gate_refuses_before_finalize_and_before_its_backfills() {
    let contract = ContractMigration {
        id: "gate-contract",
        after_fleet: 2,
        after_backfills: &["gate-backfill"],
        statements: "",
        min_reader: 2,
    };
    let before_finalize = contract_admitted(&contract, 1, |_| true).unwrap_err();
    assert_eq!(
        before_finalize,
        MigrationRefusal::ContractBeforeFinalize {
            migration: "gate-contract".to_owned(),
            recorded: 1,
            requires: 2,
        }
    );
    assert!(before_finalize.to_string().contains("lashctl finalize"));
    let before_backfills = contract_admitted(&contract, 2, |_| false).unwrap_err();
    assert_eq!(
        before_backfills,
        MigrationRefusal::ContractBeforeBackfills {
            migration: "gate-contract".to_owned(),
            pending: vec!["gate-backfill".to_owned()],
        }
    );
    assert!(
        before_backfills
            .to_string()
            .contains("lashctl migrate --phase backfill")
    );
    contract_admitted(&contract, 2, |backfill| backfill == "gate-backfill")
        .expect("finalized and backfilled");
}

#[test]
fn migration_refusals_serialize_tagged() {
    assert_eq!(
        serde_json::to_value(MigrationRefusal::BackfillBeforeFinalize {
            migration: "b".to_owned(),
            recorded: 1,
            requires: 2,
        })
        .expect("serialize"),
        serde_json::json!({"refusal":"backfill_before_finalize","migration":"b","recorded":1,"requires":2})
    );
}
