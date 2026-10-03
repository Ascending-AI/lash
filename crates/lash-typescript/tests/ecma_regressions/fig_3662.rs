//! FIG-3662: five divergences that needed no new value shape, no `this` channel, and no prototype chain — each a wrong answer from a code path that already had the right inputs.

use super::*;

// FIG-3662 self-contained fixes: five divergences that needed no new value
// shape, no `this` channel, and no prototype chain — each one is a wrong answer
// produced by a code path that already had the right inputs.

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
