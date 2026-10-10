//! What the lowered document of each language form must hold.

use lash_kernel_doc::Name;

use super::{lower, lower_in_session, main_text};
use crate::DiagnosticCode;

/// Function declarations exist from their block's start and reach each
/// other whatever their order: the one declared later is a variable the
/// earlier one's closure already shares.
#[test]
fn function_declarations_are_hoisted_and_mutually_reachable() {
    let text = main_text(
        "even(2); function even(n) { return n === 0 || odd(n - 1); } \
         function odd(n) { return n !== 0 && even(n - 1); }",
    );
    assert!(
        text.starts_with("let odd = absent\nlet even = fn("),
        "{text}"
    );
    assert!(text.contains("\nset odd = fn("), "{text}");
    assert!(text.ends_with("do apply even(absent, t19)"), "{text}");
}

/// A function that ends without `return` gives `undefined`; the kernel's
/// own default is null.
#[test]
fn a_function_without_a_return_gives_undefined() {
    assert_eq!(
        main_text("function f() {}"),
        "let f = fn(this1, args1) {\n  return absent\n}"
    );
}

/// Each pass of a `for` whose head declares with `let` has its own binding
/// when the body makes a function, and `continue` carries the pass's value
/// to the next one.
#[test]
fn a_closure_made_in_a_for_loop_captures_that_pass() {
    let text = main_text(
        "var fns = []; for (let i = 0; i < 3; i++) { if (i === 1) continue; fns.push(() => i); }",
    );
    for expected in [
        "let i_1 = 0.0",
        "  let i_2 = i_1",
        "    set i_2 = num.add(t3, 1.0)",
        "  let t5 = num.lt(i_2, 3.0)",
        "    set i_1 = i_2\n    continue",
        "    return i_2",
        "  set i_1 = i_2\n}",
    ] {
        assert!(text.contains(expected), "missing `{expected}` in\n{text}");
    }
}

/// A `switch` is a loop of one pass, so a `continue` inside it first leaves
/// that loop and then continues the loop the source means.
#[test]
fn continue_inside_a_switch_continues_the_enclosing_loop() {
    let text = main_text("var x = 0; while (x < 2) { switch (x) { case 0: x++; continue; } x++; }");
    assert!(text.contains("      set t8 = true\n      break"), "{text}");
    assert!(text.contains("  if t8 {\n    continue\n  }"), "{text}");
}

/// A `let` a function reads from an enclosing block, when the function may
/// run before the declaration, is checked where it is read. One the source
/// itself reads too early is refused before anything runs.
#[test]
fn a_binding_read_before_its_declaration_is_caught() {
    let text = main_text(
        "{ const peek = () => late; let late = f(); peek(); } function f() { return 1; }",
    );
    assert!(text.contains("let late_1 = ()"), "{text}");
    assert!(
        text.contains("let t2 = invoke ts.tdz(late_1, \"late\")"),
        "{text}"
    );
    let refused = lower("x = 1; let x;").unwrap_err();
    assert_eq!(refused.code, DiagnosticCode::TemporalDeadZone);
}

/// What the dialect refuses keeps the code it was refused under.
#[test]
fn refusals_keep_their_codes() {
    for (source, code) in [
        ("const c = 1; c = 2;", DiagnosticCode::AssignConst),
        ("missing + 1;", DiagnosticCode::UnknownBinding),
        ("new Foo();", DiagnosticCode::NewUnsupported),
        ("this.x;", DiagnosticCode::ThisUnsupported),
        ("arguments;", DiagnosticCode::ArgumentsUnsupported),
        (
            "var x; x instanceof Foo;",
            DiagnosticCode::InstanceOfUnsupported,
        ),
        ("class A {}", DiagnosticCode::ClassUnsupported),
        (
            "out: while (true) { break out; }",
            DiagnosticCode::LabelUnsupported,
        ),
    ] {
        match lower(source) {
            Err(diagnostic) => assert_eq!(diagnostic.code, code, "{source}: {diagnostic}"),
            Ok(_) => panic!("`{source}` lowered"),
        }
    }
}

/// `K-SES-001`: a cell's top-level bindings are the session's. Temporaries
/// and the bindings of inner blocks are private, a binding an earlier cell
/// left is read by name, and declaring it again assigns it.
#[test]
fn top_level_bindings_are_the_sessions() {
    let lowered = lower("const a = [1]; { let inner = a; }").unwrap();
    let private: Vec<&str> = lowered
        .document
        .private_bindings
        .iter()
        .map(Name::as_str)
        .collect();
    assert_eq!(private, ["inner_1"]);

    let lowered = lower_in_session("let seen = total + 1; let total = seen;", &["total"]);
    let Err(refused) = lowered else {
        panic!("a name read before its declaration in the same cell is refused");
    };
    assert_eq!(refused.code, DiagnosticCode::TemporalDeadZone);

    let lowered = lower_in_session("var seen = total + 1; var total = seen;", &["total"]).unwrap();
    assert_eq!(
        lowered.document.main,
        lash_kernel_doc::parse_document(&format!(
            "kernel 1\nnumbers float\nuse ts.add = @{}\nmain {{\n  let seen = invoke ts.add(total, 1.0)\n  set total = seen\n}}\n",
            lash_kernel_dialect::Library::resolve(super::library(), "ts.add").unwrap()
        ))
        .unwrap()
        .main
    );
}

/// `K-DOC-002`: the manifest lists every function the document reaches,
/// through the bodies of the helpers it calls.
#[test]
fn the_manifest_lists_functions_reached_through_helpers() {
    let lowered = lower_in_session("a + 1;", &["a"]).unwrap();
    let listed: Vec<&str> = lowered
        .document
        .manifest
        .functions
        .values()
        .map(|name| name.as_str())
        .collect();
    for name in [
        "ts.add",
        "ts.to_primitive",
        "ts.receiver",
        "kind",
        "num.add",
    ] {
        assert!(
            listed.contains(&name),
            "`{name}` is not listed in {listed:?}"
        );
    }
}

/// A statement's source span, and the label its `@label` comment gives it,
/// are annotations on the site of the statement that does the work.
#[test]
fn spans_and_labels_annotate_sites() {
    let source = "/** @label Greet — say hello */\nconsole.log('hi');\nlet z = 1;";
    let lowered = lower(source).unwrap();
    assert_eq!(lowered.annotations.dialect.as_deref(), Some("typescript"));
    let nodes = &lowered.annotations.nodes;
    let labelled: Vec<_> = nodes.iter().filter(|node| node.label.is_some()).collect();
    assert_eq!(labelled.len(), 1);
    assert_eq!(labelled[0].site.path, [1]);
    assert_eq!(labelled[0].label.as_ref().unwrap().title, "Greet");
    let call = source.find("console").unwrap();
    assert_eq!(labelled[0].data["span"][0], call);
    let last = nodes.last().unwrap();
    assert_eq!(last.site.path, [2]);
    assert_eq!(last.data["span"][0], source.find("let z").unwrap());
}

/// A member reference is read before its argument expressions run.
#[test]
fn member_reference_failure_precedes_argument_effects() {
    use super::machine::{Ended, end};
    assert_eq!(
        end(
            "let ran = false; const o = {}; try { o.missing.call(ran = true); } catch (e) {} await finish(ran);"
        ),
        Ended::Finished(lash_kernel_doc::Datum::Bool(false))
    );
}

/// Parentheses around an optional member keep its call receiver.
#[test]
fn parenthesized_optional_member_keeps_the_receiver() {
    use super::machine::{Ended, end};
    assert_eq!(
        end("const o = { x: 7, f() { return this.x; } }; await finish((o?.f)());"),
        Ended::Finished(lash_kernel_doc::Datum::Float(lash_kernel_doc::Float::new(
            7.0
        )))
    );
}

/// Strict arguments are objects with an independent length and poisoned callee.
#[test]
fn strict_arguments_have_object_brand_and_restricted_callee() {
    use super::machine::{Ended, end};
    assert_eq!(
        end(
            "function f(a) { const args = arguments; args.length = 4294967296; let poisoned = false; try { args.callee; } catch(e) { poisoned = e.name === 'TypeError'; } const lexical = () => arguments; const independent = args[0] === a; args[0] = 8; const unmapped = a === 3; const huge = args.length === 4294967296; delete args.length; args.length = 'small'; return !Array.isArray(args) && poisoned && independent && unmapped && huge && args.length === 'small' && lexical() === args; } await finish(f(3));"
        ),
        Ended::Finished(lash_kernel_doc::Datum::Bool(true))
    );
}

/// For-of observes collection mutations before requesting the next item.
#[test]
fn for_of_reads_map_and_set_iterators_live() {
    use super::machine::{Ended, end};
    assert_eq!(
        end(
            "const s = new Set([1]); let n = 0; for (const value of s) { n++; if(value === 1) s.add(2); } const m = new Map([[0, 'a']]); let c = 0; for (var entry of m) { if(entry[0] === 0 && entry[1] === 'a') m.set(1, 'b'); c++; } await finish(n === 2 && c === 2);"
        ),
        Ended::Finished(lash_kernel_doc::Datum::Bool(true))
    );
}

/// Membership includes inherited Object methods; own-property checks do not.
#[test]
fn membership_distinguishes_inherited_and_own_properties() {
    use super::machine::{Ended, end};
    assert_eq!(
        end("const o = {}; await finish(('valueOf' in o) && !Object.hasOwn(o, 'valueOf'));"),
        Ended::Finished(lash_kernel_doc::Datum::Bool(true))
    );
}

/// Strict destructuring assignment cannot write eval or arguments.
#[test]
fn strict_for_of_assignment_checks_restricted_identifiers() {
    for source in ["for ({eval} of [{}]) ;", "for ({eval = 0} of [{}]) ;"] {
        assert_eq!(lower(source).unwrap_err().code, DiagnosticCode::SyntaxError);
    }
}

/// Primitive global names are values when used as member receivers.
#[test]
fn primitive_global_member_access_uses_runtime_semantics() {
    use super::machine::{Ended, end};
    assert_eq!(
        end(
            "let caught = false; try { undefined.toString(); } catch(e) { caught = e.name === 'TypeError'; } await finish(caught);"
        ),
        Ended::Finished(lash_kernel_doc::Datum::Bool(true))
    );
}

/// Arguments retain their array iterator after acquiring their object brand.
#[test]
fn arguments_iteration_reads_live_length_and_indices() {
    use super::machine::{Ended, end};
    assert_eq!(
        end(
            "function f(a) { const copy = [...arguments]; let sum = 0; for(const value of arguments) { sum += value; if(value === 1) { arguments[1] = 2; arguments.length = 2; } } return copy[0] === 1 && sum === 3; } await finish(f(1));"
        ),
        Ended::Finished(lash_kernel_doc::Datum::Bool(true))
    );
}
