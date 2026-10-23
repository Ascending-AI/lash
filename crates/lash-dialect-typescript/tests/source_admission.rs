//! FIG-7237: source admission is total; erasure preserves explicit observations.

#[path = "../src/tests/linked.rs"]
mod linked;
#[expect(
    dead_code,
    reason = "shared machine fixture also supports leaf and reopen laws"
)]
#[path = "../src/tests/pages.rs"]
mod pages;
#[expect(
    dead_code,
    reason = "this differential uses only ordinary source execution"
)]
#[path = "../shared/helper_differential/runner.rs"]
mod runner;
#[path = "../shared/source_admission.rs"]
mod source_admission;

use runner::{Case, Runner};
use serde_json::json;

/// FIG-7237 / K-DOC-005: bounded malformed source either crosses the typed
/// refusal boundary or produces a structurally valid, serializable document.
#[test]
fn malformed_and_ambiguous_source_has_typed_admission_outcomes() {
    let mut harness = source_admission::Harness::new().expect("source admission library");
    let mut state = 7237_u64;
    for feature in 0_u16..256 {
        let mut data = vec![u8::try_from(feature).expect("one byte feature")];
        for _ in 0..31 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            data.push(state.to_le_bytes()[0]);
        }
        harness.check(&data);
    }
    println!("256 inputs, {:?}", harness.counts);
    assert!(harness.counts.admitted > 0);
    assert!(harness.counts.source_refusals > 0);
}

fn case(id: &str, source: String) -> Case {
    Case {
        id: id.into(),
        dialect: "typescript".into(),
        source,
        bits: 0,
        stride: 0,
        nanoseconds: 0,
    }
}

/// FIG-7237 / ADR 0044: compare only this deterministic admitted subset.
/// The JavaScript side is authored erasure, never regex-stripped TypeScript.
/// Literal observations independently pin both sides; Node has no enum role.
#[test]
fn deterministic_typescript_erasure_preserves_observations() {
    let runner = Runner::new("typescript").expect("machine fixture");
    let mut pairs = 0;
    for n in 0..16 {
        let variants = [
            (
                format!("type N = number; const x: N = {n}; await host.observe(x);"),
                format!("const x = {n}; await host.observe(x);"),
            ),
            (
                format!(
                    "function id<T>(x: T): T {{ return x; }} await host.observe(id<number>({n}));"
                ),
                format!("function id(x) {{ return x; }} await host.observe(id({n}));"),
            ),
            (
                format!(
                    "const x = ({{v: {n}}} satisfies {{v: number}}); await host.observe((x as {{v: number}})!.v);"
                ),
                format!("const x = {{v: {n}}}; await host.observe(x.v);"),
            ),
            (
                format!(
                    "const x: {{v?: number}} | null = {{v: {n}}}; await host.observe(x?.v ?? 0);"
                ),
                format!("const x = {{v: {n}}}; await host.observe(x?.v ?? 0);"),
            ),
            (
                format!("const id = <T,>(x: T): T => x; await host.observe(id<number>({n}));"),
                format!("const id = (x) => x; await host.observe(id({n}));"),
            ),
            (
                format!(
                    "interface Box<T> {{ v: T }} const x: Box<number> = {{v: {n}}}; await host.observe(x.v);"
                ),
                format!("const x = {{v: {n}}}; await host.observe(x.v);"),
            ),
        ];
        let expected = json!({"type":"observations", "value":[{
            "type":"float", "bits":format!("{:016x}", f64::from(n).to_bits())
        }]});
        for (typed, erased) in variants {
            for source in [typed, erased] {
                let case = case("erasure", source);
                let observed = runner.run(&case).expect("admitted deterministic source");
                assert_eq!(observed.result, expected, "{}", case.source);
            }
            pairs += 1;
        }
        // TS parses a < b > (c) as a generic call, even with these spaces.
        // Its erased JS is a(c), not the same comparison-looking tokens.
        let generic = format!(
            "function a<T>(x: T): T {{ return x; }} type b = number; const c = {n}; await host.observe(a < b > (c));"
        );
        let erased =
            format!("function a(x) {{ return x; }} const c = {n}; await host.observe(a(c));");
        for source in [generic, erased] {
            let case = case("generic-ambiguity", source);
            let observed = runner.run(&case).expect("admitted generic call");
            assert_eq!(observed.result, expected, "{}", case.source);
        }
        pairs += 1;
        // Parentheses force a comparison; both authored forms keep them.
        let comparison = format!(
            "const a: number = {n}; const b = 20; const c = 0; await host.observe((a < b) > c);"
        );
        let erased =
            format!("const a = {n}; const b = 20; const c = 0; await host.observe((a < b) > c);");
        for source in [comparison, erased] {
            let case = case("comparison", source);
            let observed = runner.run(&case).expect("admitted comparison");
            assert_eq!(
                observed.result,
                json!({"type":"observations", "value":[{"type":"bool", "value":true}]}),
                "{}",
                case.source
            );
        }
        pairs += 1;
    }
    println!("{pairs} erasure pairs, {} machine executions", pairs * 2);
}

/// FIG-7237: ordinary enum initializers execute once in source order, const-enum member reads inline constants.
#[test]
fn enum_initializer_order_and_const_enum_inlining_are_observable() {
    let runner = Runner::new("typescript").expect("machine fixture");
    let case = case(
        "enum-order",
        r#"
        let log = '';
        function mark(label: string, n: number): number { log += label; return n; }
        enum E { A = mark('a', 2), B = mark('b', 3) }
        const enum C { A = 4, B }
        await host.observe(log === 'ab' && E.A === 2 && E.B === 3 && C.B === 5);
    "#
        .into(),
    );
    let observed = runner.run(&case).expect("admitted enum fixture");
    assert_eq!(
        observed.result,
        json!({"type":"observations", "value":[{"type":"bool", "value":true}]})
    );
}

/// FIG-7237 regression: an enum is a record, so numeric reverse lookup must
/// not emit list.len/list.get even in a statically unreachable fast path.
#[test]
fn enum_reverse_lookup_uses_record_semantics() {
    let runner = Runner::new("typescript").expect("machine fixture");
    let case = case(
        "enum-reverse",
        "enum E { A = 2 } await host.observe(E[2] === 'A');".into(),
    );
    let observed = runner.run(&case).expect("enum reverse lookup is admitted");
    assert_eq!(
        observed.result,
        json!({"type":"observations", "value":[{"type":"bool", "value":true}]})
    );
}
