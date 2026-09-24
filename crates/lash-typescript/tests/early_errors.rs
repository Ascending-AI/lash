//! ECMA-262's early errors in the front end (FIG-3650), and the valid programs
//! it used to refuse with one (FIG-3651).
//!
//! Sources spell a backslash as `§`, so every Unicode escape below reaches the
//! parser exactly as written.

use lash_typescript::DiagnosticCode as Code;
use lashlang::{AbilityOp, AbilityResult, ExecutionHost, ExecutionHostError, State, Value};

struct Host;

impl ExecutionHost for Host {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            AbilityOp::Finish(value) => Ok(AbilityResult::Value(value)),
            _ => Err(ExecutionHostError::new("unsupported early-error ability")),
        }
    }
}

fn js(source: &str) -> String {
    source.replace('§', "\\")
}

fn finished(source: &str) -> Value {
    let source = js(source);
    let program = lash_typescript::testing::compile(&source)
        .unwrap_or_else(|error| panic!("`{source}` should compile: {error}"));
    match futures::executor::block_on(lashlang::execute(&program, &mut State::new(), &Host)) {
        Ok(lashlang::ExecutionOutcome::Finished(value)) => value,
        other => panic!("`{source}`: expected finish, got {other:?}"),
    }
}

fn rejected(source: &str) -> lash_typescript::Diagnostic {
    let source = js(source);
    lash_typescript::validate(&source).expect_err(&source)
}

fn assert_early_errors(sources: &[&str]) {
    for source in sources {
        let error = rejected(source);
        assert_eq!(error.code, Code::SyntaxError, "`{source}`: {error}");
    }
}

#[test]
fn reserved_words_are_not_identifiers_however_spelled() {
    assert_early_errors(&[
        "var this = 1;",
        "var x = ({ default }) => 1;",
        "var x = ({ extends }) => 1;",
        "var §u0062reak = 1;",
        "var §u{62}§u{72}eak = 1;",
        "var §u0069mplements = 1;",
        "var §u0079ield = 1;",
        "var §u006eull = 1;",
        "var x = ({ §u0063onst }) => 1;",
        "var x = { §u0063onst } = { const: 1 };",
        "if (true) { f§u0061lse; }",
        "async function f() { var §u0061wait; }",
        "var f = async () => { void §u0061wait; };",
        "for (var x o§u0066 []) ;",
    ]);
}

#[test]
fn identifiers_are_spelled_by_the_identifier_rules() {
    assert_early_errors(&["var §u200D;", "var §u200C = 1;", "var §u{00_76} = 1;"]);
}

#[test]
fn an_escaped_reserved_word_is_a_property_name() {
    assert_eq!(
        finished(
            "const o = { bre§u0061k: 1, §u0069f: 2 }; finish(o.bre§u0061k + o['if'] + o.i§u0066);"
        ),
        Value::Number(5.0)
    );
    assert_eq!(
        finished("const o = { case: 1 }; o.c§u0061se = 4; finish(o['case']);"),
        Value::Number(4.0)
    );
    assert_eq!(
        finished("const o = { t§u0068is() { return 6; } }; finish(o['this']());"),
        Value::Number(6.0)
    );
    // A property named with an escape inside a regular expression literal is
    // no word at all; the literal is untouched.
    assert_eq!(
        finished("finish(/bre§u0061k/.test('break'));"),
        Value::Bool(true)
    );
}

#[test]
fn await_is_an_identifier_outside_async_functions_of_a_script() {
    assert_eq!(
        finished("var await = 1; finish(await + 1);"),
        Value::Number(2.0)
    );
    assert_eq!(
        finished("function f(await) { return await * 3; } finish(f(2));"),
        Value::Number(6.0)
    );
    assert_eq!(
        finished("const await = 5; function g() { return await; } finish(g());"),
        Value::Number(5.0)
    );
    // Inside an async function it is still reserved, and a cell that awaits
    // at its top level reads `await` as the operator throughout.
    assert_early_errors(&[
        "var await = 1; async function f() { var await = 2; }",
        "var await = 1; async function f() { return await; }",
    ]);
    let error = rejected("function f() { return await (g()); } function g() { return 1; }");
    assert_eq!(error.code, Code::UnknownBinding, "{error}");
}

#[test]
fn a_lexical_name_is_declared_once_per_scope() {
    assert_early_errors(&[
        "{ let f; var f; }",
        "{ const f = 0; { var f; } }",
        "{ var f; function f() {} }",
        "{ async function f() {} var f; }",
        "{ function f() {} function f() {} }",
        "function x() { { let f; var f; } }",
        "let a = 1; var a = 2;",
        "let a = 1; function a() {}",
        "switch (0) { case 1: let f; default: var f; }",
        "switch (0) { case 1: function f() {} default: function f() {} }",
        "for (let x of []) { var x; }",
        "for (const x in {}) { var x; }",
        "for (let x = 0; x < 1; x++) { var x; }",
        "try {} catch (x) { let x; }",
        "try {} catch (e) { function e() {} }",
        "try {} catch ([e]) { var e; }",
        "function f(a) { let a; }",
        "function f(a, a) {}",
        "const g = (a) => { const a = 1; };",
    ]);
}

#[test]
fn declarations_in_distinct_scopes_coexist() {
    assert_eq!(
        finished("{ let f = 1; } var f = 2; finish(f);"),
        Value::Number(2.0)
    );
    assert_eq!(
        finished("function g(a) { { let a = 2; return a; } } finish(g(1));"),
        Value::Number(2.0)
    );
    assert_eq!(
        finished("let n = 0; try { throw 3; } catch (e) { var e2 = e; n = e2; } finish(n);"),
        Value::Number(3.0)
    );
}

/// ECMA-262 allows each of these; `tsc --strict` rejects each (TS2393,
/// TS2300), so the dialect refuses them by name. An early error elsewhere in
/// the same cell still wins.
#[test]
fn a_function_redeclared_in_its_var_scope_is_refused_by_name() {
    for source in [
        "function f() { return 1; } function f() { return 2; }",
        "var f; function f() {}",
        "function f() {} var f = 1;",
        "function g(f) { function f() {} }",
        "function f() {} { var f; }",
    ] {
        let error = rejected(source);
        assert_eq!(
            error.code,
            Code::FunctionRedeclarationUnsupported,
            "`{source}`: {error}"
        );
        assert!(error.message.contains("TS2393"), "{error}");
    }
    assert_early_errors(&["function f() {} function f() {} { let x; var x; }"]);
}

#[test]
fn delete_takes_a_property_reference() {
    assert_early_errors(&["var o = {}; delete o;", "var o = {}; delete (o);"]);
    for source in [
        "delete 1;",
        "delete (1 + 2);",
        "var f = () => 1; delete f();",
    ] {
        let error = rejected(source);
        assert_eq!(error.code, Code::DeleteNonReferenceUnsupported, "{error}");
        assert!(error.message.contains("TS2703"), "{error}");
    }
    assert_eq!(
        finished("const o = { a: 1 }; const d = delete (o.a); finish(d && !('a' in o));"),
        Value::Bool(true)
    );
}

#[test]
fn literals_hold_no_legacy_or_malformed_escapes() {
    let line_separator = char::from_u32(0x2028).map(String::from).unwrap_or_default();
    let paragraph_separator = char::from_u32(0x2029).map(String::from).unwrap_or_default();
    let regex_with_line_separator = format!("var r = /a{line_separator}/;");
    let regex_with_paragraph_separator = format!("var r = /{paragraph_separator}/;");
    assert_early_errors(&[
        "var s = '§8';",
        "var s = '§9';",
        "var s = '§08';",
        "var s = '§u{1F_639}';",
        "var t = `§8`;",
        "var t = `§01`;",
        "var t = `§u{1F_639}`;",
        &regex_with_line_separator,
        &regex_with_paragraph_separator,
        "var o = { async\n f() {} };",
        "var f = async () => { var g = (a = await/r/g) => a; };",
        "for (false; false) { break; }",
        "for (; false) { break; }",
        "for (let i = 0; i < 1) { break; }",
    ]);
    assert_eq!(
        finished("finish('§0' === String.fromCharCode(0) && '§0'.length === 1);"),
        Value::Bool(true)
    );
}

#[test]
fn script_unknown_matches_the_code_points_no_script_claims() {
    assert_eq!(
        finished(
            "finish([/§p{Script=Unknown}/u.test(String.fromCodePoint(0x378)), /§p{sc=Zzzz}/u.test('A'), /§p{scx=Unknown}/u.test(String.fromCodePoint(0xE000)), /§P{Script_Extensions=Zzzz}/u.test('A')].join());"
        ),
        Value::String("true,false,true,true".into())
    );
}
