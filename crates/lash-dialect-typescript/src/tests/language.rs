//! What the lowered document of each language form must hold.

use lash_kernel_doc::Name;

use super::{lower, lower_in_session};
use crate::DiagnosticCode;

/// A function that ends without `return` gives `undefined` through both a
/// direct call and its function value; the kernel's own default is null.
#[test]
fn a_function_without_a_return_gives_undefined() {
    use super::machine::{Ended, end};
    assert_eq!(
        end("function f() {} const g = f; await finish(f() === undefined && g() === undefined);"),
        Ended::Finished(lash_kernel_doc::Datum::Bool(true))
    );
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
            "kernel 1\nnumbers float\nuse ts.add = @{}\nmain {{\n  let seen = ts.add(total, 1.0)\n  set total = seen\n}}\n",
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

/// A statement's source span is mapped at the site of the statement that
/// does the work. A former `@label` doc comment is ordinary trivia: it
/// locates nothing.
#[test]
fn spans_map_sites_and_label_comments_are_trivia() {
    let source = "/** @label Greet — say hello */\nconsole.log('hi');\nlet z = 1;";
    let lowered = lower(source).unwrap();
    let map = lowered.source_map.unwrap();
    assert_eq!(map.language, lash_kernel_doc::SourceLanguage::TypeScript);
    let start = |path: &[u32]| {
        map.sites
            .iter()
            .find(|located| located.site.path == path)
            .map(|located| located.ranges[0].start)
    };
    assert_eq!(start(&[0]), source.find("console"));
    let last = map.sites.last().unwrap();
    assert_eq!(last.site.path, [1]);
    assert_eq!(start(&[1]), source.find("let z"));
    assert!(
        map.sites
            .iter()
            .all(|located| located.ranges[0].start >= source.find('\n').unwrap())
    );
}

/// A member reference is read before its argument expressions run.
#[test]
fn member_reference_failure_precedes_argument_effects() {
    use super::machine::{Ended, end};
    for method in ["get", "set", "has"] {
        assert_eq!(
            end(&format!(
                "function receiver() {{ return null; }} let ran = false; try {{ receiver().{method}(ran = true); }} catch(e) {{}} await finish(ran);"
            )),
            Ended::Finished(lash_kernel_doc::Datum::Bool(false))
        );
    }
    // An accessor is refused at admission, including one named like a Map method.
    assert_eq!(
        super::lower("const o = { get get() { throw 1; } }; o.get(2);")
            .unwrap_err()
            .code,
        crate::DiagnosticCode::AccessorUnsupported
    );
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

/// ECMA-262 FunctionDeclarationInstantiation: missing positions are undefined,
/// defaults run left to right only for undefined, rest is fresh, and strict
/// arguments retain the supplied length and values after the call returns.
#[test]
fn parameters_preserve_defaults_rest_and_escaping_arguments() {
    use super::machine::{Ended, end};
    assert_eq!(
        end(r#"
            let order = '';
            const hoisted = even(4);
            function even(n) { return n === 0 || odd(n - 1); }
            function odd(n) { return n !== 0 && even(n - 1); }
            function mark(name, value) { order += name; return value; }
            function f(a = mark('a', 1), b = mark('b', a + 1), c, ...rest) {
                return {a, b, c, rest, args: arguments, lexical: () => arguments};
            }
            const missing = f();
            const supplied = f(null, undefined, 3, 4, 5);
            supplied.rest[0] = 8; supplied.args[0] = 9;
            const again = f(7, 8);
            let correct = hoisted && order === 'abb' && missing.a === 1 && missing.b === 2;
            correct &&= missing.c === undefined && missing.rest.length === 0 && missing.args.length === 0;
            correct &&= missing.lexical() === missing.args && supplied.a === null && supplied.b === 1;
            correct &&= supplied.c === 3 && supplied.rest[0] === 8 && supplied.args[3] === 4;
            correct &&= supplied.args.length === 5 && supplied.lexical() === supplied.args;
            correct &&= again.a === 7 && again.b === 8 && again.args[0] === 7 && again.args !== supplied.args;
            await finish(correct);
            "#),
        Ended::Finished(lash_kernel_doc::Datum::Bool(true)),
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

/// Object rest copies a string's enumerable indices into a new ordinary object.
#[test]
fn object_rest_copies_string_indices_into_a_fresh_object() {
    use super::machine::{Ended, end};
    assert_eq!(
        end(
            "const {0: first, ...rest} = 'foo'; await finish(first === 'f' && rest[1] === 'o' && rest[2] === 'o' && Object.keys(rest).length === 2 && rest instanceof Object);"
        ),
        Ended::Finished(lash_kernel_doc::Datum::Bool(true)),
    );
}

/// A function's `name` is its own or the one named evaluation gives it, and
/// its `length` counts the parameters before the first default or rest.
#[test]
fn a_function_has_the_name_and_length_ecma_gives_it() {
    super::remaining_builtins::agrees(
        "function f(a, b,) {} const g = function (a, b = 1, c) {}; const h = (...rest) => rest; const own = function inner() {}; const [d = () => 0] = []; let v: any = 1; v &&= function () {}; const o = { m(x) {} }; async function af(a) {} await finish(f.name === 'f' && f.length === 2 && g.name === 'g' && g.length === 1 && h.name === 'h' && h.length === 0 && own.name === 'inner' && d.name === 'd' && v.name === 'v' && o.m.name === 'm' && o.m.length === 1 && af.name === 'af' && af.length === 1 && (function () {}).name === '');",
    );
}

/// A function's `name` and `length` are own, read-only and configurable:
/// a write raises TypeError, and once deleted the function reads the
/// values Function.prototype holds.
#[test]
fn a_functions_name_and_length_are_read_only_and_configurable() {
    super::remaining_builtins::agrees(
        "function f(a) {} let rejected = false; try { (f as any).length = 3; } catch (e) { rejected = e instanceof TypeError; } let listed = false; for (const key in f) { listed = true; } await finish(rejected && f.length === 1 && !listed && Object.hasOwn(f, 'length') && delete (f as any).length && !Object.hasOwn(f, 'length') && f.length === 0 && delete (f as any).name && f.name === '');",
    );
}

/// A bound function is named for its target and takes the target's length
/// less the arguments bound. Reading an iterator method's metadata leaves
/// the iterator where it was.
#[test]
fn bound_functions_and_iterator_methods_have_their_own_name_and_length() {
    super::remaining_builtins::agrees(
        "function bar(x, y) {} const once = bar.bind(null, 1); const twice = once.bind(null, 2, 3); const it = [7].values(); await finish(once.name === 'bound bar' && once.length === 1 && twice.name === 'bound bound bar' && twice.length === 0 && Math.max.bind(null).length === 2 && it.next.name === 'next' && it.next.length === 0 && it.next().value === 7);",
    );
}

/// A function the program made is a token with an empty path, compared by
/// the closure it carries: it equals itself however it is reached, and two
/// functions never compare equal, as Map keys or array members either.
#[test]
fn distinct_functions_are_never_equal() {
    super::remaining_builtins::agrees(
        "const a = () => 1; const b = () => 1; function f() {} const g = f; const m = new Map([[a, 1]]); await finish(a !== b && a === a && f === g && Object.is(f, g) && !Object.is(a, b) && [a].includes(a) && ![a].includes(b) && [b, a].indexOf(a) === 1 && m.get(a) === 1 && !m.has(b) && new Set([a, b, a]).size === 2 && typeof a === 'function');",
    );
}

/// K-CLO-001: folding a conditional keeps each source function's calling convention.
#[test]
fn conditional_functions_keep_their_written_signatures() {
    for source in [
        "const f = true ? ((left: number) => left) : ((right: string) => right);",
        "true ? ((left: number) => left) : ((right: string) => right);",
        "let holder = {}; holder.f = true ? ((left: number) => left) : ((right: string) => right);",
    ] {
        let lowered = super::lower(source).unwrap();
        assert_eq!(lowered.written.len(), 2);
        for site in lowered.written.keys() {
            let mut node = lash_kernel_doc::Node::Block(&lowered.document.main);
            for child in &site.path {
                node = node.children()[*child as usize];
            }
            assert!(
                matches!(
                    node,
                    lash_kernel_doc::Node::Expr(lash_kernel_doc::Expr::Closure(_))
                ),
                "{site}"
            );
        }
        assert!(
            lowered
                .written
                .values()
                .any(|written| written.signature.contains("left"))
        );
        assert!(
            lowered
                .written
                .values()
                .any(|written| written.signature.contains("right"))
        );
    }
}
