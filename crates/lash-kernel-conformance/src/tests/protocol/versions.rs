//! Version admission and the shipped migration's document cases.

#[cfg(feature = "synthetic-next")]
use lash_kernel_doc::parse_definition;
use lash_kernel_doc::{Document, KernelVersion};
use lash_kernel_state::ParkedRun;
use lash_kernel_vm::{KernelMachine, Machine, PreparedLibrary};

use super::{Case, registry};

pub(super) fn check(rule: &str, case: &Case) {
    let library = PreparedLibrary::new(registry());
    if rule == "K-VER-003" {
        // Unsupported versions win over malformed payloads: none of these
        // readers may enter the unsupported version's decoder.
        let refusal =
            Document::from_json(r#"{"manifest":{"kernel":4294967295},"main":false}"#).unwrap_err();
        assert!(refusal.to_string().contains("4294967295"));
        let refusal =
            lash_kernel_doc::FunctionDefinition::from_json(r#"{"kernel":4294967295,"name":false}"#)
                .unwrap_err();
        assert!(refusal.to_string().contains("4294967295"));
        let refusal =
            ParkedRun::from_json(br#"{"run":{"kernel":4294967295},"tasks":false}"#).unwrap_err();
        assert!(refusal.to_string().contains("4294967295"));
        let mut seen = 0;
        let actual = crate::machine::run_case::<KernelMachine>(&library, case, &mut |mut at| {
            seen += 1;
            let saved = at.machine.export().unwrap();
            assert_eq!(saved.run.kernel, KernelVersion::One.number());
            let mut unsupported = saved.clone();
            unsupported.run.kernel = u32::MAX;
            assert!(KernelMachine::import(at.program.clone(), at.bounds, unsupported).is_err());
            let encoded = saved.to_json().unwrap();
            assert_eq!(ParkedRun::from_json(&encoded).unwrap(), saved);
            Ok(KernelMachine::import(at.program.clone(), at.bounds, saved).unwrap())
        })
        .unwrap();
        assert_eq!(seen, 1);
        case.expected.check(&actual).unwrap();
        // A registered function of another version cannot serve this
        // document's manifest, even when its signature is unchanged.
        #[cfg(feature = "synthetic-next")]
        {
            let mut library = lash_kernel_doc::FunctionRegistry::clone(library.registry());
            let mut definition = parse_definition(
                "function identity(x: Any) -> Any\nkernel 1\ncharge 1\nbody { return x }",
            )
            .unwrap();
            definition.kernel = KernelVersion::SyntheticNext.number();
            let id = library.register(definition, None).unwrap();
            let document = lash_kernel_doc::parse_document(&format!("kernel 1\nnumbers by_spelling\nuse identity = @{id}\nmain {{ let r = invoke identity(null) finish r }}")).unwrap();
            assert!(
                lash_kernel_check::admit(&document, &lash_kernel_check::Environment::new(&library))
                    .is_err()
            );
        }
        return;
    }
    #[cfg(not(feature = "synthetic-next"))]
    {
        assert!(
            lash_kernel_migrate::migration_from(KernelVersion::One).is_none(),
            "the 1.0 baseline has no predecessor migration"
        );
        let mut runner =
            crate::MachineRunner::<KernelMachine>::new(std::sync::Arc::clone(library.registry()));
        crate::check_case(&mut runner, case).unwrap();
    }
    #[cfg(feature = "synthetic-next")]
    {
        let migration = lash_kernel_migrate::migration_from(KernelVersion::One).unwrap();
        let mut library = lash_kernel_doc::FunctionRegistry::clone(library.registry());
        let definition = parse_definition(
            "function identity(x: Any) -> Any\nkernel 1\ncharge 1\nbody { return x }",
        )
        .unwrap();
        let old = library.register(definition.clone(), None).unwrap();
        let next = (migration.definition)(&definition).unwrap();
        assert_eq!(next.kernel, migration.to.number());
        assert_ne!(next.identity().unwrap(), old);
        lash_kernel_migrate::migrate_registry(&mut library, migration).unwrap();
        let mut case = case.clone();
        if rule == "K-VER-004" {
            case.document = format!(
                "kernel 1\nnumbers by_spelling\neffect echo(n: Int) -> Int\nuse identity = @{old}\nmain {{ let x = invoke identity(1) do perform echo(x) as Int finish null }}"
            );
        }
        let document = lash_kernel_doc::parse_document(&case.document).unwrap();
        let rewritten = (migration.document)(&document, &library).unwrap();
        assert_eq!(rewritten.document.manifest.kernel, migration.to.number());
        if rule == "K-VER-004" {
            assert_eq!(rewritten.functions[&old], next.identity().unwrap());
        }
        for survivor in rewritten.correspondence.entries() {
            assert!(document.node(&survivor.from).is_some());
            assert!(rewritten.document.node(&survivor.to).is_some());
        }
        if rule == "K-VER-004" {
            let refusal =
                (migration.document)(&document, &lash_kernel_doc::FunctionRegistry::new())
                    .unwrap_err();
            assert!(
                matches!(refusal, lash_kernel_migrate::DocumentRefusal::FunctionNotHeld { function } if function == old)
            );
        }
        let check = crate::check_migration::<KernelMachine>(
            migration,
            &std::sync::Arc::new(library),
            &case,
        )
        .unwrap();
        assert_eq!(check.migrated.len(), case.environment.deliveries.len());
    }
}
