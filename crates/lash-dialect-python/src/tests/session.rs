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
