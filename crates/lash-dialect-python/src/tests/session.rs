//! Cells of one session (`K-SES-001`).

use super::machine;

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

    let lowered = machine::lower("total = 1\nprint(total + 1)\n").expect("a plain cell");
    let private: Vec<&str> = lowered
        .document
        .private_bindings
        .iter()
        .map(lash_kernel_doc::Name::as_str)
        .collect();
    assert!(!private.contains(&"total"), "{private:?}");
    assert!(
        !private.is_empty(),
        "the temporaries of `print` are private"
    );
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
            &Environment {
                library: machine::library(),
                effects: &BTreeMap::new(),
                controls: &BTreeMap::new(),
                bindings: &bindings,
                functions: &std::collections::BTreeMap::new(),
            },
        )
        .expect_err("reject an old reserved binding");
        assert_eq!(error.code, "PY_SHADOWS_BUILTIN");
        assert!(
            error.repairs.iter().any(|repair| repair.contains(name)),
            "{error}"
        );
    }
}
