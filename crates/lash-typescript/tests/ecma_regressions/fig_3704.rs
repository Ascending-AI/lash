//! FIG-3704: stdlib follow-ups — a set-like record's guest callbacks stay a refusal only when the algorithm reaches one.

use super::*;

/// A record that passes GetSetRecord validation — numeric `size`, callable
/// `has` and `keys` — still cannot be a Set argument: those members are guest
/// closures and a synchronous builtin cannot invoke them, so the method
/// refuses only when the algorithm actually reaches a callback (FIG-3704).
#[test]
fn set_like_objects_with_guest_callbacks_stay_a_refusal() {
    for source in [
        "const o={size:2,has:()=>true,keys:()=>[9]}; finish(new Set([1]).union(o));",
        "const o={size:2,has:()=>true,keys:()=>[9]}; finish(new Set([1]).isSubsetOf(o));",
    ] {
        let error =
            execute(source).expect_err("a set-like record's guest callbacks cannot be invoked");
        assert!(
            error.to_string().contains("TS_METHOD_UNSUPPORTED"),
            "{source}: {error}"
        );
    }
    // An empty `this` never reaches a callback, so the validated set-like
    // still answers ECMA's result.
    assert_eq!(
        finished(
            "const o={size:2,has:()=>true,keys:()=>[9]}; finish(new Set().isSubsetOf(o)+'|'+new Set().isDisjointFrom(o));"
        ),
        Value::String("true|true".into())
    );
}
