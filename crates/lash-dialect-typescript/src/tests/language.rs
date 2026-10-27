//! What the lowered document of each language form must hold.

use lash_kernel_value::Name;

use super::{lower, lower_in_session};
use crate::DiagnosticCode;

/// FIG-7254: the deleted kernel grammar owns no ordinary identifier.
#[test]
fn k_is_an_ordinary_binding_and_reference() {
    assert_eq!(
        lash_kernel_dialect::FrontEnd::reserves(&crate::TypeScript::default(), "k"),
        None
    );
    for source in [
        "const k = 1; k;",
        "const { k } = { k: 1 }; k;",
        "function k(k: number): number { return k; } k(1);",
        "for (const k of [1, 2]) { k; }",
        r"const \u006b = 1; k;",
    ] {
        lower(source).unwrap_or_else(|error| panic!("{source}: {error}"));
    }
    lower_in_session("k;", &["k"]).expect("an earlier cell's k is ordinary too");
}

/// FIG-7254: unrelated source cannot supply repair advice for a parse error.
#[test]
fn parser_repairs_ignore_unrelated_raise_text() {
    let plain = crate::parse("const = ;").unwrap_err();
    for prefix in [
        "// raise\n",
        "/* raise */\n",
        "const raise = 0;\n",
        "' raise ';\n",
    ] {
        let error = crate::parse(&format!("{prefix}const = ;")).unwrap_err();
        assert_eq!(error.code(), plain.code());
        assert_eq!(error.kind(), plain.kind());
        assert_eq!(error.message, plain.message);
        assert_eq!(error.suggestions, plain.suggestions);
    }
}

/// What the dialect refuses keeps the code it was refused under.
#[test]
fn refusals_keep_their_codes() {
    for (source, code) in [
        ("const c = 1; c = 2;", DiagnosticCode::AssignConst),
        ("missing + 1;", DiagnosticCode::UnknownBinding),
        ("new Foo();", DiagnosticCode::UnknownBinding),
        ("arguments;", DiagnosticCode::ArgumentsUnsupported),
        ("var x; x instanceof Foo;", DiagnosticCode::UnknownBinding),
        ("class A {}", DiagnosticCode::ClassUnsupported),
    ] {
        match lower(source) {
            Err(diagnostic) => assert_eq!(diagnostic.code(), code, "{source}: {diagnostic}"),
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
    assert_eq!(refused.code(), DiagnosticCode::TemporalDeadZone);

    let lowered = lower_in_session("var seen = total + 1; var total = seen;", &["total"]).unwrap();
    let main = lowered.document.main.as_ref().unwrap();
    assert!(
        matches!(main.first(), Some(lash_kernel_doc::Stmt::Let { name, .. }) if name.as_str() == "seen")
    );
    assert!(
        matches!(main.last(), Some(lash_kernel_doc::Stmt::Assign { place: lash_kernel_doc::Place::Variable(name), .. }) if name.as_str() == "total")
    );
}

/// `K-DOC-002`: the manifest lists every function the document reaches,
/// through the bodies of the helpers it calls.
/// A statement's source span is mapped at the site of the statement that
/// does the work. A former `@label` doc comment is a JSDoc of tags alone:
/// ordinary trivia that locates nothing and, in a cell or a workflow module,
/// captions nothing.
#[test]
fn spans_map_sites_and_label_comments_are_trivia() {
    let source = "/** @label Greet — say hello */\nconsole.log('hi');\nlet z = 1;";
    let lowered = lower(source).unwrap();
    assert!(lowered.captions.is_empty());
    let map = lowered.source_map.unwrap();
    assert_eq!(map.language, lash_kernel_doc::SourceLanguage::TypeScript);
    let start = |path: &[u32]| {
        map.sites
            .iter()
            .find(|located| located.site.path == path)
            .map(|located| located.location.ranges()[0].start)
    };
    assert_eq!(start(&[0]), source.find("console"));
    let last = map.sites.last().unwrap();
    assert_eq!(last.site.path, [1]);
    assert_eq!(start(&[1]), source.find("let z"));
    assert!(
        map.sites
            .iter()
            .all(|located| located.location.ranges()[0].start >= source.find('\n').unwrap())
    );

    let effects = super::machine::effects();
    let environment_namespaces = crate::tests::linked::namespaces(&effects, super::controls())
        .expect("the test's effects are operations");
    let environment =
        lash_kernel_dialect::Environment::new(super::library(), &environment_namespaces);
    let module = |source: &str| {
        crate::lower(source, &environment, lash_kernel_dialect::Mode::Module)
            .unwrap()
            .captions
    };
    let labelled = "/** @label Greet — say hello */\nexport async function greet(): void {\n  /** @label Say it */\n  console.log('hi');\n}\n";
    assert!(module(labelled).is_empty());
    // Without its tag, the same block is a caption.
    assert_eq!(module(&labelled.replace("@label ", "")).entries().len(), 2);
}

/// A member reference is read before its argument expressions run.
#[test]
fn member_reference_failure_precedes_argument_effects() {
    use super::machine::{Ended, end};
    for method in ["get", "set", "has"] {
        assert_eq!(
            end(&format!(
                "function receiver() {{ return null; }} let ran = false; try {{ receiver().{method}(ran = true); }} catch(e) {{}} await control.finish(ran);"
            )),
            Ended::Finished(lash_kernel_doc::Datum::Bool(false))
        );
    }
    // An accessor is refused at admission, including one named like a Map method.
    assert_eq!(
        super::lower("const o = { get get() { throw 1; } }; o.get(2);")
            .unwrap_err()
            .code(),
        crate::DiagnosticCode::AccessorUnsupported
    );
    assert_eq!(
        end(
            "let ran = false; const o = {}; try { o.missing.call(ran = true); } catch (e) {} await control.finish(ran);"
        ),
        Ended::Finished(lash_kernel_doc::Datum::Bool(false))
    );
}

/// Strict arguments are objects with an independent length and poisoned callee.
#[test]
fn strict_arguments_have_object_brand_and_restricted_callee() {
    use super::machine::{Ended, end};
    assert_eq!(
        end(
            "function f(a) { const args = arguments; args.length = 4294967296; let poisoned = false; try { args.callee; } catch(e) { poisoned = e.name === 'TypeError'; } const lexical = () => arguments; const independent = args[0] === a; args[0] = 8; const unmapped = a === 3; const huge = args.length === 4294967296; delete args.length; args.length = 'small'; return !Array.isArray(args) && poisoned && independent && unmapped && huge && args.length === 'small' && lexical() === args; } await control.finish(f(3));"
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
            await control.finish(correct);
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
            "const s = new Set([1]); let n = 0; for (const value of s) { n++; if(value === 1) s.add(2); } const m = new Map([[0, 'a']]); let c = 0; for (var entry of m) { if(entry[0] === 0 && entry[1] === 'a') m.set(1, 'b'); c++; } await control.finish(n === 2 && c === 2);"
        ),
        Ended::Finished(lash_kernel_doc::Datum::Bool(true))
    );
}

/// Membership includes inherited Object methods; own-property checks do not.
#[test]
fn membership_distinguishes_inherited_and_own_properties() {
    use super::machine::{Ended, end};
    assert_eq!(
        end(
            "const o = {}; await control.finish(('valueOf' in o) && !Object.hasOwn(o, 'valueOf'));"
        ),
        Ended::Finished(lash_kernel_doc::Datum::Bool(true))
    );
}

/// Primitive global names are values when used as member receivers.
#[test]
fn primitive_global_member_access_uses_runtime_semantics() {
    use super::machine::{Ended, end};
    assert_eq!(
        end(
            "let caught = false; try { undefined.toString(); } catch(e) { caught = e.name === 'TypeError'; } await control.finish(caught);"
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
            "function f(a) { const copy = [...arguments]; let sum = 0; for(const value of arguments) { sum += value; if(value === 1) { arguments[1] = 2; arguments.length = 2; } } return copy[0] === 1 && sum === 3; } await control.finish(f(1));"
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
            "const {0: first, ...rest} = 'foo'; await control.finish(first === 'f' && rest[1] === 'o' && rest[2] === 'o' && Object.keys(rest).length === 2 && rest instanceof Object);"
        ),
        Ended::Finished(lash_kernel_doc::Datum::Bool(true)),
    );
}

/// A function's `name` is its own or the one named evaluation gives it, and
/// its `length` counts the parameters before the first default or rest.
#[test]
fn a_function_has_the_name_and_length_ecma_gives_it() {
    super::remaining_builtins::agrees(
        "function f(a, b,) {} const g = function (a, b = 1, c) {}; const h = (...rest) => rest; const own = function inner() {}; const [d = () => 0] = []; let v: any = 1; v &&= function () {}; const o = { m(x) {} }; async function af(a) {} await control.finish(f.name === 'f' && f.length === 2 && g.name === 'g' && g.length === 1 && h.name === 'h' && h.length === 0 && own.name === 'inner' && d.name === 'd' && v.name === 'v' && o.m.name === 'm' && o.m.length === 1 && af.name === 'af' && af.length === 1 && (function () {}).name === '');",
    );
}

/// FIG-7179: ordinary defaults and rest retain JavaScript calling semantics;
/// their public signatures admit omission and their ECMA length is zero.
#[test]
fn default_and_rest_functions_run_with_their_ecma_length() {
    use super::machine::{Ended, end};
    for source in [
        "function f(a = 1, c) { return c === undefined ? a : a + c; } await control.finish(f.length === 0 && f() === 1 && f(undefined, 2) === 3);",
        "function f(...xs) { return xs.length; } await control.finish(f.length === 0 && f() === 0 && f(1, 2, 3) === 3);",
    ] {
        assert_eq!(
            end(source),
            Ended::Finished(lash_kernel_doc::Datum::Bool(true)),
            "{source}",
        );
    }
}

/// A function's `name` and `length` are own, read-only and configurable:
/// a write raises TypeError, and once deleted the function reads the
/// values Function.prototype holds.
#[test]
fn a_functions_name_and_length_are_read_only_and_configurable() {
    super::remaining_builtins::agrees(
        "function f(a) {} let rejected = false; try { (f as any).length = 3; } catch (e) { rejected = e instanceof TypeError; } let listed = false; for (const key in f) { listed = true; } await control.finish(rejected && f.length === 1 && !listed && Object.hasOwn(f, 'length') && delete (f as any).length && !Object.hasOwn(f, 'length') && f.length === 0 && delete (f as any).name && f.name === '');",
    );
}

/// A bound function is named for its target and takes the target's length
/// less the arguments bound. Reading an iterator method's metadata leaves
/// the iterator where it was.
#[test]
fn bound_functions_and_iterator_methods_have_their_own_name_and_length() {
    super::remaining_builtins::agrees(
        "function bar(x, y) {} const once = bar.bind(null, 1); const twice = once.bind(null, 2, 3); const it = [7].values(); await control.finish(once.name === 'bound bar' && once.length === 1 && twice.name === 'bound bound bar' && twice.length === 0 && Math.max.bind(null).length === 2 && it.next.name === 'next' && it.next.length === 0 && it.next().value === 7);",
    );
}

/// A function the program made is a token with an empty path, compared by
/// the closure it carries: it equals itself however it is reached, and two
/// functions never compare equal, as Map keys or array members either.
#[test]
fn distinct_functions_are_never_equal() {
    super::remaining_builtins::agrees(
        "const a = () => 1; const b = () => 1; function f() {} const g = f; const m = new Map([[a, 1]]); await control.finish(a !== b && a === a && f === g && Object.is(f, g) && !Object.is(a, b) && [a].includes(a) && ![a].includes(b) && [b, a].indexOf(a) === 1 && m.get(a) === 1 && !m.has(b) && new Set([a, b, a]).size === 2 && typeof a === 'function');",
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
            let mut node = lash_kernel_doc::Node::Block(
                lowered.document.main.as_ref().expect("authored main"),
            );
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

/// FIG-7284 / K-EVAL-001: pure syntax evaluates operands once and preserves call references.
#[test]
fn pure_expression_forms_preserve_references_and_order() {
    use super::machine::{Ended, end};
    assert_eq!(
        end(r#"
        const log = [];
        function note(x) { log.push(x); return x; }
        const obj = { x: 7, f() { return this; } };
        const unbound = (note(1), obj.f)();
        debugger;
        function ordinary(undefined) { return new.target; }
        const deleted = delete (note(2) + note(3));
        let missing = null;
        const skipped = delete missing?.[note(99)];
        const present = { x: 1 };
        const removed = delete (present?.[note("x")]);
        await control.finish(unbound === undefined && this === undefined
            && ordinary(99) === undefined && deleted && skipped && removed
            && !("x" in present) && log.join(",") === "1,2,3,x");
    "#),
        Ended::Finished(lash_kernel_doc::Datum::Bool(true))
    );
    assert!(super::lower("new.target;").is_err());
    assert!(super::lower("arguments;").is_err());
}

/// FIG-7284 / K-FORM-017: named departures unwind finalizers before the target epilogue.
#[test]
fn labeled_exits_preserve_finally_and_loop_epilogues() {
    use super::machine::{Ended, end};
    assert_eq!(
        end(r#"
        let log = "";
        outer: alias: for (let i = 0; i < 3; log += "u", i++) {
            try {
                for (let j = 0; j < 2; j++) {
                    try { if (i < 2) continue alias; break outer; }
                    catch (e) { log += "wrong"; }
                    finally { log += "f"; }
                }
            } finally { log += i; }
            log += "wrong";
        }
        region: { try { break region; } finally { log += "b"; } log += "wrong"; }
        another: while (true) { try { continue another; } finally { break another; } }
        let count = 0;
        last: for (; count < 3; count++) {
            try { break last; } finally { if (count === 0) continue last; }
        }
        await control.finish(log === "f0uf1uf2b" && count === 1);
    "#),
        Ended::Finished(lash_kernel_doc::Datum::Bool(true))
    );
}

/// FIG-7284: a tag receives frozen arrays once per source site and keeps its receiver.
#[test]
fn tagged_templates_reuse_the_site_object() {
    use super::machine::{Ended, end};
    assert_eq!(
        end(r#"
        let first;
        let raw;
        let stable = true;
        let calls = 0;
        let order = "";
        const receiver = { tag(strings, value) {
            calls++;
            stable = stable && this === receiver && Array.isArray(strings)
                && Array.isArray(strings.raw) && Object.isFrozen(strings) && Object.isFrozen(strings.raw);
            if (first === undefined) { first = strings; raw = strings.raw; }
            else stable = stable && first === strings && raw === strings.raw;
            stable = stable && strings[0] === "a\n" && strings.raw[0] === "a\\n" && value === calls;
            return strings;
        } };
        function mark() { order += "v"; return calls + 1; }
        function site() { order += "t"; return receiver.tag`a\n${mark()}z`; }
        site(); site();
        const entry = Object.getOwnPropertyDescriptor(first, '0');
        const length = Object.getOwnPropertyDescriptor(first, 'length');
        const rawProperty = Object.getOwnPropertyDescriptor(first, 'raw');
        const rawEntry = Object.getOwnPropertyDescriptor(raw, '0');
        stable = stable && Object.getOwnPropertyNames(first).join(',') === '0,1,length,raw'
            && Object.getOwnPropertyNames(raw).join(',') === '0,1,length'
            && Object.keys(first).join(',') === '0,1';
        stable = stable && entry.value === first[0] && entry.enumerable && !entry.writable && !entry.configurable;
        stable = stable && length.value === 2 && !length.enumerable && !length.writable && !length.configurable;
        stable = stable && rawProperty.value === raw && !rawProperty.enumerable && !rawProperty.writable && !rawProperty.configurable;
        stable = stable && rawEntry.value === raw[0] && rawEntry.enumerable && !rawEntry.writable && !rawEntry.configurable;
        stable = stable && !Object.hasOwn(first, 'items') && !Object.hasOwn(first, '__lash_brand');
        let refusals = 0;
        try { first[0] = "changed"; } catch (e) { refusals++; }
        try { raw.push("changed"); } catch (e) { refusals++; }
        try { delete first.raw; } catch (e) { refusals++; }
        const identity = value => value;
        const invalid = identity`\8`;
        const other = identity`a\n${0}z`;
        await control.finish(stable && calls === 2 && order === "tvtv" && refusals === 3
            && invalid[0] === undefined && invalid.raw[0] === "\\8" && first !== other
            && String(first) === "a\n,z" && JSON.stringify(invalid) === "[null]");
    "#),
        Ended::Finished(lash_kernel_doc::Datum::Bool(true))
    );
    assert_eq!(
        end(
            "const identity = value => value; const text = identity`hello`; await control.finish({ nested: [text, text.raw] });"
        ),
        Ended::Finished(lash_kernel_doc::Datum::Record(vec![(
            "nested".into(),
            lash_kernel_doc::Datum::List(vec![
                lash_kernel_doc::Datum::List(vec![lash_kernel_doc::Datum::Text("hello".into())]),
                lash_kernel_doc::Datum::List(vec![lash_kernel_doc::Datum::Text("hello".into())]),
            ])
        )]))
    );
}

/// FIG-7284: erased scaffolding creates no value; legal var-scope declarations share one binding.
#[test]
fn erased_declarations_do_not_create_runtime_values() {
    use super::machine::{Ended, end};
    assert_eq!(
        end(r#"
        declare const absentValue: number;
        declare enum Ambient { Value }
        declare namespace Types { interface Data { x: number } }
        declare class TypeOnly { x: number; }
        function overloaded(x: number): number;
        function overloaded(x) { return x + 1; }
        function winner() { return 1; }
        const before = winner;
        var winner;
        function winner() { return 2; }
        type ErasedAlias = number;
        interface ErasedInterface { x: number }
        export { winner as publicWinner, overloaded, ErasedAlias, ErasedInterface };
        function parameter(value) { function value() { return 3; } var value; return value(); }
        await control.finish(before === winner && winner() === 2 && overloaded(4) === 5 && parameter(0) === 3);
    "#),
        Ended::Finished(lash_kernel_doc::Datum::Bool(true))
    );
    for source in [
        "declare const absentValue: number; absentValue;",
        "declare enum Ambient { Value } Ambient;",
        "function missing(x: number): number;",
        "let duplicate; function duplicate() {}",
    ] {
        assert!(super::lower(source).is_err(), "{source}");
    }
    let namespaces = Default::default();
    let environment = lash_kernel_dialect::Environment::new(super::library(), &namespaces);
    let lowered = crate::lower(
        "function original(x: number): number { return x; } export { original as renamed };",
        &environment,
        lash_kernel_dialect::Mode::Module,
    )
    .unwrap();
    lash_kernel_doc::validate_document(&lowered.document, super::library()).unwrap();
    assert_eq!(
        lowered.document.exports[&Name::new("original")],
        lowered.document.exports[&Name::new("renamed")]
    );
}
