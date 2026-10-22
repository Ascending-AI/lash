//! Version admission and the shipped migration's document cases.

use lash_kernel_doc::{Document, KernelVersion};

use lash_kernel_vm::{KernelMachine, Machine, PreparedLibrary};

use super::{Case, registry};

pub(super) fn check(rule: &str, case: &Case) {
    let library = PreparedLibrary::new(registry());
    if rule == "K-VER-003" {
        // Unsupported versions win over malformed payloads: none of these
        // readers may enter the unsupported version's decoder.
        let refusal = Document::from_json(
            r#"{"requires":{"natives":[],"kernels":[4294967295]},"main":false}"#,
        )
        .unwrap_err();
        assert!(refusal.to_string().contains("4294967295"));
        let refusal =
            lash_kernel_doc::FunctionDefinition::from_json(r#"{"kernel":4294967295,"name":false}"#)
                .unwrap_err();
        assert!(refusal.to_string().contains("4294967295"));
        let mut seen = 0;
        let actual = crate::machine::run_case::<KernelMachine>(&library, case, &mut |mut at| {
            seen += 1;
            let saved = at.machine.seal(crate::pages::BUDGET).unwrap().unwrap();
            assert_eq!(
                lash_kernel_vm::kernel_internals::sealed_root(&saved).kernel,
                KernelVersion::One.number()
            );
            let mut unsupported = lash_kernel_vm::kernel_internals::sealed_root(&saved).clone();
            unsupported.kernel = u32::MAX;
            assert!(matches!(
                crate::pages::open::<KernelMachine>(
                    at.program.clone(),
                    at.bounds,
                    &unsupported,
                    at.source,
                ),
                Err(lash_kernel_vm::OpenError::KernelVersion { .. })
            ));
            let encoded = lash_vocabulary::tokens::encode_body(
                lash_kernel_vm::kernel_internals::sealed_root(&saved),
            );
            let decoded = lash_vocabulary::tokens::decode_body::<
                lash_kernel_state::paged::CheckpointRoot,
            >(&encoded)
            .unwrap();
            assert_eq!(lash_vocabulary::tokens::encode_body(&decoded), encoded);
            Ok(crate::pages::open::<KernelMachine>(
                at.program.clone(),
                at.bounds,
                &decoded,
                at.source,
            )
            .unwrap())
        })
        .unwrap();
        assert_eq!(seen, 1);
        case.expected.check(&actual).unwrap();
        return;
    }
    let mut runner =
        crate::MachineRunner::<KernelMachine>::new(std::sync::Arc::clone(library.registry()));
    crate::check_case(&mut runner, case).unwrap();
}
