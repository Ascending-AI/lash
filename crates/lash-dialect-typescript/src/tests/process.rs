//! Cell functions remain ordinary code when process tools are offered.

use std::collections::BTreeMap;

use lash_kernel_doc::print_document;
use lash_vocabulary::{EffectName, Name, Param, Signature, Type};

use super::machine;

/// The laws' tools and the process effects.
fn effects() -> BTreeMap<EffectName, Signature> {
    let mut effects = machine::effects();
    for name in ["processes.start", "processes.await"] {
        effects.insert(
            EffectName::new(name).expect("an effect's name"),
            Signature {
                params: vec![Param {
                    name: Name::new("input"),
                    ty: Type::Any,
                    optional: false,
                }],
                result: Type::Any,
            },
        );
    }
    effects
}

fn lower(source: &str) -> Result<lash_kernel_dialect::Lowered, crate::Diagnostic> {
    super::lower_against(source, &[], &effects())
}

#[test]
fn awaiting_a_value_waits_for_the_process_a_handle_names() {
    let document = lower(
        "const handle = await processes.start({ definition: async () => 1 });\nconst ended = await handle;",
    )
    .expect("the cell lowers")
    .document;
    let text = print_document(&document);
    assert!(
        text.contains(&format!(
            "perform {}(",
            super::linked_inspection::operation_slot(&document, "processes.await")
                .expect("operation import")
                .0
        )),
        "`await handle` waits as `processes.await` does:\n{text}"
    );
    // Where no process effect is offered, an await is an await.
    let plain = super::lower_with_effects("const got = await Promise.resolve(3);")
        .expect("the cell lowers")
        .document;
    assert!(super::linked_inspection::operation_slot(&plain, "processes.await").is_none());
}

/// FIG-6361: split returns List(Any). Every method sharing the guarded
/// array dispatch must link as well as execute, including its fallback.
#[test]
fn join_on_a_split_result_links_in_an_ordinary_async_function() {
    let source = r#"
const work = async () => {
  const w = "a b".split(" ");
  return [w.join("-"), w.includes("a"), w.indexOf("b"), w.lastIndexOf("a"),
    w.slice(1).join(""), w.map(x => x).join(""), w.filter(x => true).join("")];
};
"#;
    for declaration in ["const w", "const w: any[]"] {
        let source = source.replace("const w =", &format!("{declaration} ="));
        let source = format!("{source}\nconsole.log(JSON.stringify(await work()));");
        let document = lower(&source)
            .expect("the definition lowers")
            .document
            .into_document();
        lash_kernel_check::derive(&document, super::library()).expect("the definition body links");
        let recorded = machine::run(&source, &[]);
        assert_eq!(recorded.lines(), ["[\"a-b\",true,1,0,\"b\",\"ab\",\"ab\"]"]);
    }
}

/// CELL-PLAIN (F1): offered tools never turn ordinary cell functions into entries.
#[test]
fn a_capturing_async_cell_function_is_plain_code_with_or_without_process_tools() {
    let source = "const limit = 3; const w = async () => limit;";
    let plain = super::lower_against(source, &[], &machine::effects())
        .expect("ordinary async closure")
        .document;
    let offered = lower(source)
        .expect("the same closure with process tools")
        .document;
    assert!(plain.exports.is_empty());
    assert!(offered.exports.is_empty());
    assert_eq!(plain, offered, "tools do not change function lowering");
}
