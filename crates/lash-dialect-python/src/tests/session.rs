//! Cells of one session (`K-SES-001`).

use super::machine;

/// FIG-6049: host history records expose string keys like Python dicts,
/// including nested records and the typed exception for a missing key.
#[test]
fn history_records_support_dict_reads_and_typed_key_errors() {
    use lash_kernel_doc::{Datum, Name};
    use lash_kernel_vm::{End, RunError};
    use std::collections::BTreeMap;
    let bindings = super::pages::Session::seeded([(
        Name::new("history"),
        Datum::List(vec![
            Datum::Null,
            Datum::Record(vec![(
                "final_output".into(),
                Datum::Record(vec![
                    ("attachment".into(), Datum::Text("retained-value".into())),
                    ("empty".into(), Datum::Record(Vec::new())),
                ]),
            )]),
        ]),
    )]);
    let source = "\
output = history[1]['final_output']
print(output['attachment'])
print('attachment' in output, 'missing' in output, 1 in output)
print(output.get('attachment'), output.get('missing', 'fallback'), output.get('missing'))
print(output.keys(), list(output), [key for key in output], len(output))
print(output['empty'].keys(), list(output['empty']), len(output['empty']), bool(output['empty']))
try:
    output['missing']
except KeyError as error:
    print(type(error).__name__, error.args, str(error))
history[1]['missing']
";
    let (lines, end) = machine::drive(source, BTreeMap::new(), bindings, |_, _, _| {
        panic!("reading history does not park")
    });
    assert_eq!(
        lines,
        [
            "retained-value",
            "True False False",
            "retained-value fallback None",
            "['attachment', 'empty'] ['attachment', 'empty'] ['attachment', 'empty'] 2",
            "[] [] 0 False",
            "KeyError ('missing',) 'missing'",
        ],
        "{end:?}"
    );
    let End::Error(RunError::Uncaught {
        error: Datum::Error(error),
        ..
    }) = end
    else {
        panic!("a missing history key raises a typed error: {end:?}");
    };
    assert_eq!(error.kind, "KeyError");
    assert_eq!(error.message, "'missing'");
    assert_eq!(
        error.data,
        Datum::Tuple(vec![
            Datum::Tuple(vec![Datum::Text("missing".into())]),
            Datum::Null,
        ])
    );
}

/// A top-level name is the session's: the next cell reads and rebinds it
/// without declaring it again, and a temporary is the cell's own.
#[test]
fn top_level_names_are_session_bindings() {
    let lines = machine::session(&[
        "total = 1\nitems = [total]\n",
        "total += 1\nitems.append(total)\nprint(total, items)\n",
        "print(total + len(items))\n",
    ]);
    assert_eq!(lines, ["2 [1, 2]", "4"]);
}

/// FIG-6680 / K-SES-001: cell carry uses a module loop target even when
/// no later statement reads it. An empty loop leaves its target unbound.
#[test]
fn loop_targets_remain_module_bindings_for_the_next_cell() {
    for cleanup in [
        "",
        "import asyncio\nasync def work():\n    return 1\nawait asyncio.create_task(work())\n",
    ] {
        let cell = format!("{cleanup}for last in [1, 2]:\n    pass\nfor empty in []:\n    pass\n");
        let lines = machine::session(&[
            &cell,
            "print(last)\ntry:\n    print(empty)\nexcept NameError:\n    print('unbound')\n",
        ]);
        assert_eq!(lines, ["2", "unbound"]);
    }
}

/// A name an earlier cell declared and never assigned is still unbound.
#[test]
fn a_binding_never_assigned_stays_unbound_in_the_next_cell() {
    let lines = machine::session(&[
        "if False:\n    later = 1\n",
        "try:\n    print(later)\nexcept NameError as error:\n    print(error)\n",
    ]);
    assert_eq!(lines, ["name 'later' is not defined"]);
}

/// FIG-5779: module bindings, including block targets, cannot mask built-ins.
#[test]
fn session_bindings_cannot_shadow_builtins() {
    for (name, source) in [
        ("len", "len = 1\n"),
        ("len", "len: int = 1\n"),
        ("len", "len, count = [1, 2]\n"),
        ("len", "def len():\n    return 1\n"),
        ("len", "if True:\n    len = 1\n"),
        ("len", "for len in [1, 2]:\n    pass\n"),
        ("open", "open = 1\n"),
        ("ValueError", "ValueError = 1\n"),
        ("ValueError", "class ValueError(Exception):\n    pass\n"),
        (
            "len",
            "try:\n    pass\nexcept ValueError as len:\n    pass\n",
        ),
    ] {
        let error = machine::lower(source).expect_err(source);
        assert_eq!(error.code, "PY_SHADOWS_BUILTIN", "{error}");
        assert_eq!(error.kind, lash_kernel_dialect::DiagnosticKind::Refusal);
        assert_eq!(
            error.message,
            format!("`{name}` is a built-in; a top-level binding cannot reuse its name")
        );
        assert!(
            error
                .repairs
                .iter()
                .any(|repair| repair.contains(&format!("{name}_"))),
            "{error}"
        );
        machine::lower(&source.replace(name, &format!("{name}_")))
            .expect("the renamed binding lowers");
    }
}

/// Local names never enter the session.
#[test]
fn nested_scope_builtin_shadowing_stays_local() {
    for source in [
        "def local(len, finish):\n    return len + finish\nlocal(1, 2)\n",
        "def local():\n    len = 1\n    return len\nlocal()\n",
        "[len for len in [1, 2]]\n",
    ] {
        machine::lower(source).expect("local shadowing stays allowed");
    }
}

/// Every external session name is checked before it participates in resolution.
#[test]
fn restored_session_bindings_cannot_mask_builtins() {
    use lash_kernel_dialect::Environment;
    use lash_kernel_doc::Name;
    use std::collections::{BTreeMap, BTreeSet};

    {
        let name = "len";
        let bindings = BTreeSet::from([Name::new(name)]);
        let error = crate::lower(
            "1\n",
            &Environment::new(
                machine::library(),
                &lash_kernel_dialect::Namespaces::of_signatures(
                    &Default::default(),
                    &Default::default(),
                )
                .expect("the test's effects are operations"),
            ),
            lash_kernel_dialect::Mode::Cell(lash_kernel_dialect::CellEnvironment {
                bindings: &bindings,
                functions: &BTreeMap::new(),
            }),
        )
        .expect_err("reject an old reserved binding");
        assert_eq!(error.code, "PY_SHADOWS_BUILTIN");
        assert!(
            error.repairs.iter().any(|repair| repair.contains(name)),
            "{error}"
        );
    }
}

/// FIG-5824: control remains reserved without any offered control tools.
#[test]
fn control_is_reserved_without_control_tools() {
    let error = machine::lower("control = 1\n").expect_err("control is reserved");
    assert_eq!(error.code, "PY_SHADOWS_BUILTIN");
    assert!(
        error
            .repairs
            .iter()
            .any(|repair| repair.contains("control_"))
    );
}

/// FIG-6524: parameters and earlier cells shadow namespaces in their scope.
#[test]
fn tool_discovery_resolves_lexical_values_before_namespaces() {
    use lash_kernel_dialect::FrontEnd as _;
    let bindings = std::collections::BTreeSet::from([lash_kernel_doc::Name::new("previous")]);
    let calls = crate::Python::default().tool_calls(
        "text = 'phantom.call()'\ndef order(items):\n    items.sort(key=lambda item: item['key'])\nprevious.sort()\n[2, 1].sort()\nprint(text)\n# phantom.comment()\nawait items.sort({})\nawait other.fetch({})\n",
        &lash_kernel_dialect::Environment::new(
machine::library(),
&lash_kernel_dialect::Namespaces::of_signatures(&Default::default(), &Default::default()).expect("the test's effects are operations"),
),
        lash_kernel_dialect::CellEnvironment { bindings: &bindings, functions: &Default::default() },
    ).expect("discover calls in their scopes");
    assert_eq!(
        calls,
        std::collections::BTreeSet::from(["items.sort".into(), "other.fetch".into()])
    );
}
