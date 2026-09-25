//! FIG-3662: five divergences that needed no new value shape, no `this` channel, and no prototype chain — each a wrong answer from a code path that already had the right inputs.

use super::*;

// FIG-3662 self-contained fixes: five divergences that needed no new value
// shape, no `this` channel, and no prototype chain — each one is a wrong answer
// produced by a code path that already had the right inputs.

#[test]
fn fig3662_parse_float_uses_the_ecma_decimal_grammar() {
    for source in [
        "finish(parseFloat('infinity'));",
        "finish(parseFloat('INFINITY'));",
        "finish(parseFloat('.x'));",
        "finish(parseFloat(''));",
        "finish(parseFloat('e3'));",
        "finish(parseFloat('+'));",
    ] {
        assert!(
            matches!(finished(source), Value::Number(number) if number.is_nan()),
            "{source}"
        );
    }
    assert_eq!(
        finished("finish(parseFloat('Infinity'));"),
        Value::Number(f64::INFINITY)
    );
    assert_eq!(
        finished("finish(parseFloat(' -Infinity '));"),
        Value::Number(f64::NEG_INFINITY)
    );
    assert_eq!(
        finished("finish(parseFloat('5.e3'));"),
        Value::Number(5000.0)
    );
    assert_eq!(
        finished("finish(parseFloat('1.5xyz'));"),
        Value::Number(1.5)
    );
}

#[test]
fn fig3662_to_exponential_rounds_exactly() {
    // ECMA rounds the exact binary value and takes the larger magnitude on a
    // tie — `25` is a halfway case that must render `3e+1`, not Rust's
    // round-half-even `2e+1`.
    for (source, expected) in [
        ("finish((25).toExponential(0));", "3e+1"),
        ("finish((-25).toExponential(0));", "-3e+1"),
        ("finish((12345).toExponential(3));", "1.235e+4"),
        ("finish((0).toExponential(2));", "0.00e+0"),
        (
            "finish((123.456).toExponential(20));",
            "1.23456000000000003070e+2",
        ),
    ] {
        assert_eq!(finished(source), Value::String(expected.into()), "{source}");
    }
}

#[test]
fn fig3662_computed_record_keys_use_ecma_to_string() {
    assert_eq!(
        finished(
            "var ok={[1e55]:'B',[1.2]:'A',[-0]:'D',[NaN]:'G',[Infinity]:'E'};\
             finish(ok['1e+55']+'|'+ok['1.2']+'|'+ok[0]+'|'+ok.NaN+'|'+ok.Infinity);"
        ),
        Value::String("B|A|D|G|E".into())
    );
}

#[test]
fn fig3662_set_intersection_iterates_the_smaller_side() {
    for (source, expected) in [
        (
            "finish([...(new Set([3,2,1,0])).intersection(new Set([1,3,5]))].join(','));",
            "1,3",
        ),
        (
            "finish([...(new Set([1,3,5])).intersection(new Set([3,2,1,0]))].join(','));",
            "1,3",
        ),
        (
            "finish([...(new Set([3,2,1])).intersection(new Set([1,3,5,7]))].join(','));",
            "3,1",
        ),
    ] {
        assert_eq!(finished(source), Value::String(expected.into()), "{source}");
    }
}

#[test]
fn fig3662_cyclic_values_throw_inside_a_cell_and_refuse_at_the_boundary() {
    // `JSON.stringify` on a cycle throws Node's catchable TypeError.
    assert_eq!(
        finished(
            "var a=[]; a.push(a);\
             var r=(function(x){try{JSON.stringify(x);return 'no-throw'}catch(e){return e instanceof TypeError}})(a);\
             a.pop(); finish(r);"
        ),
        Value::Bool(true)
    );
    // A binding that still holds the cycle when the cell ends cannot be
    // written down; the boundary refusal names itself.
    let error = execute("const a = []; a.push(a); finish(1);")
        .expect_err("a retained cycle refuses at durable capture");
    assert!(
        error.to_string().contains("TS_CYCLIC_VALUE_UNSUPPORTED"),
        "{error}"
    );
}
