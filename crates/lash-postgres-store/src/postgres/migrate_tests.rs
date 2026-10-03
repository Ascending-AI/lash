use super::*;

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
