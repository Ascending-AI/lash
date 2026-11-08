//! The r56 probes on the bytecode core: each lowered program at slices 1, 7
//! and 50, against literal prints, end, effects, charge and the charge at
//! every slice return. Where the base is wrong the literal is derived by
//! rule (probes/README.md): steps per `K-MACH-002` (R3, R4), hooks per R2,
//! coverage per `K-CHG-007` and R5, and no slice return without a ready
//! task (`K-MACH-005`).

use std::sync::Arc;

use lash_kernel_value::package::{Fill, Link};
use lash_kernel_vm::{Charge, CoreMachine, PreparedLibrary, Program};

use crate::{Shard, check_program};

const PROBE_FILES: &[(&str, &str)] = &[
    ("r56-c1.json", include_str!("../../probes/clo/r56-c1.json")),
    ("r56-c2.json", include_str!("../../probes/clo/r56-c2.json")),
    ("r56-c3.json", include_str!("../../probes/clo/r56-c3.json")),
    ("r56-c4.json", include_str!("../../probes/clo/r56-c4.json")),
    (
        "r56-c4b.json",
        include_str!("../../probes/clo/r56-c4b.json"),
    ),
    ("r56-c5.json", include_str!("../../probes/clo/r56-c5.json")),
    ("r56-c6.json", include_str!("../../probes/clo/r56-c6.json")),
    (
        "r56-exc1.json",
        include_str!("../../probes/err/r56-exc1.json"),
    ),
    (
        "r56-exc2.json",
        include_str!("../../probes/err/r56-exc2.json"),
    ),
    (
        "r56-exc3.json",
        include_str!("../../probes/err/r56-exc3.json"),
    ),
    (
        "r56-exc4.json",
        include_str!("../../probes/err/r56-exc4.json"),
    ),
    (
        "r56-exc5.json",
        include_str!("../../probes/err/r56-exc5.json"),
    ),
    ("r56-s1.json", include_str!("../../probes/eval/r56-s1.json")),
    (
        "r56-empty.json",
        include_str!("../../probes/fn/r56-empty.json"),
    ),
    ("r56-m1.json", include_str!("../../probes/fn/r56-m1.json")),
    ("r56-m2.json", include_str!("../../probes/fn/r56-m2.json")),
    (
        "r56-e4a.json",
        include_str!("../../probes/form/r56-e4a.json"),
    ),
    (
        "r56-e4b.json",
        include_str!("../../probes/form/r56-e4b.json"),
    ),
    (
        "r56-e4c.json",
        include_str!("../../probes/form/r56-e4c.json"),
    ),
    (
        "r56-e4c_nop.json",
        include_str!("../../probes/form/r56-e4c_nop.json"),
    ),
    (
        "r56-e4d.json",
        include_str!("../../probes/form/r56-e4d.json"),
    ),
    ("r56-i1.json", include_str!("../../probes/iter/r56-i1.json")),
    (
        "r56-i1d.json",
        include_str!("../../probes/iter/r56-i1d.json"),
    ),
    ("r56-i2.json", include_str!("../../probes/iter/r56-i2.json")),
    ("r56-i3.json", include_str!("../../probes/iter/r56-i3.json")),
    ("r56-i4.json", include_str!("../../probes/iter/r56-i4.json")),
    ("r56-i5.json", include_str!("../../probes/iter/r56-i5.json")),
    ("r56-i6.json", include_str!("../../probes/iter/r56-i6.json")),
    ("r56-i7.json", include_str!("../../probes/iter/r56-i7.json")),
    (
        "r56-i7d.json",
        include_str!("../../probes/iter/r56-i7d.json"),
    ),
    ("r56-i8.json", include_str!("../../probes/iter/r56-i8.json")),
    ("r56-k1.json", include_str!("../../probes/key/r56-k1.json")),
    ("r56-b1.json", include_str!("../../probes/num/r56-b1.json")),
    ("r56-f1.json", include_str!("../../probes/num/r56-f1.json")),
    ("r56-f2.json", include_str!("../../probes/num/r56-f2.json")),
    ("r56-t1.json", include_str!("../../probes/task/r56-t1.json")),
    ("r56-t2.json", include_str!("../../probes/task/r56-t2.json")),
    ("r56-t3.json", include_str!("../../probes/task/r56-t3.json")),
    ("r56-t4.json", include_str!("../../probes/task/r56-t4.json")),
    ("r56-u1.json", include_str!("../../probes/text/r56-u1.json")),
];

/// Links `text` with the Python and ECMAScript services filling its helper
/// operations, and a conformance contract for its own imports.
fn linked(library: &PreparedLibrary, text: &str) -> Result<Program, String> {
    let document =
        Arc::new(lash_kernel_doc::parse_document(text).map_err(|error| format!("{error:?}"))?);
    let services = || {
        lash_kernel_services::TYPESCRIPT
            .iter()
            .chain(lash_kernel_services::PYTHON.iter())
    };
    let helper_fills = lash_kernel_doc::helper_imports(&document, library.helpers().as_ref())
        .map_err(|error| error.to_string())?
        .into_iter()
        .map(|(id, import)| {
            let operation = services()
                .find(|operation| operation.name == import.name)
                .ok_or_else(|| format!("unknown helper operation {}", import.name))?;
            Ok((
                id,
                lash_kernel_value::HelperFill {
                    import,
                    fill: Fill::Operation(operation.requirement.clone()),
                },
            ))
        })
        .collect::<Result<_, String>>()?;
    let fills = document
        .imports
        .iter()
        .map(|(slot, import)| {
            let lash_kernel_value::ImportKind::Operation { signature } = &import.kind else {
                return Err("a probe imports only operations".to_owned());
            };
            let requirement = services()
                .find(|operation| operation.name == import.name)
                .map(|operation| operation.requirement.clone())
                .unwrap_or_else(|| {
                    lash_kernel_value::OperationRequirement::new(
                        signature.clone(),
                        lash_kernel_value::ContractId::new("conformance"),
                    )
                });
            Ok((*slot, Fill::Operation(requirement)))
        })
        .collect::<Result<_, String>>()?;
    let link = Link {
        helper_fills,
        document: document.identity(),
        fills,
    };
    Program::prepare(
        link.identity(),
        std::collections::BTreeMap::from([(document.identity(), document)]),
        &std::collections::BTreeMap::from([(link.identity(), link)]),
        lash_kernel_doc::heap::PageId([7; 32]),
        library.clone(),
        lash_kernel_doc::Selection::Fresh,
    )
    .map_err(|error| error.to_string())
}

#[test]
fn every_r56_probe_holds_on_the_bytecode_core() {
    let release = lash_kernel_releases::helper_release().expect("helper release");
    let library = PreparedLibrary::with_helpers(
        Arc::new(crate::installed_functions().expect("installed functions")),
        Arc::clone(&release.helpers),
    );
    let mut failures = Vec::new();
    for (file, text) in PROBE_FILES {
        let shard: Shard = serde_json::from_str(text).expect(file);
        for case in &shard.cases {
            let checked = linked(&library, &case.document).and_then(|program| {
                check_program::<CoreMachine>(program, case, Charge::Exact)
                    .map(drop)
                    .map_err(|error| error.to_string())
            });
            if let Err(why) = checked {
                failures.push(format!("{file} {}: {why}", case.name));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
