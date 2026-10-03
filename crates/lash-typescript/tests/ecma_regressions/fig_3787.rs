//! FIG-3787: function call, apply and bind, and re-attached built-in
//! methods, run with ECMA receivers.

use super::*;

/// A built-in method detached and re-attached to a plain object runs against
/// the object — the receiver is the ECMA `this`, not the prototype it came
/// from.
#[test]
fn reattached_builtins_run_against_the_new_receiver() {
    for (source, expected) in [
        // pop on an empty array-like answers undefined and writes length 0.
        (
            "var o = {}; o.pop = Array.prototype.pop; finish(String(o.pop()) + ':' + o.length);",
            Value::String("undefined:0".into()),
        ),
        (
            "var o = {0:'a',1:'b',length:2}; o.pop = Array.prototype.pop; finish(String(o.pop()) + ':' + o.length);",
            Value::String("b:1".into()),
        ),
        (
            "var o = {}; o.push = Array.prototype.push; finish(o.push('x') + ':' + o.length + ':' + o[0]);",
            Value::String("1:1:x".into()),
        ),
        (
            "var o = {0:'a',1:'b',length:2}; o.join = Array.prototype.join; finish(o.join('-'));",
            Value::String("a-b".into()),
        ),
        // String methods coerce a non-string receiver through ToString.
        (
            "var o = {toString:function(){return 'AB';}}; o.toLowerCase = String.prototype.toLowerCase; finish(o.toLowerCase());",
            Value::String("ab".into()),
        ),
        // Callback-driven methods on a re-attached receiver.
        (
            "var o = {0:'a',1:'b',length:2}; o.map = Array.prototype.map; finish(o.map(function(v){return v + '!';}).join(','));",
            Value::String("a!,b!".into()),
        ),
        (
            "var o = {0:3,1:1,2:2,length:3}; o.sort = Array.prototype.sort; o.sort(function(a,b){return a-b;}); finish(o[0]+','+o[1]+','+o[2]);",
            Value::String("1,2,3".into()),
        ),
        (
            "var o = {0:'a',1:'b',length:2}; o.filter = Array.prototype.filter; finish(o.filter(function(v){return v==='b';}).join(','));",
            Value::String("b".into()),
        ),
        (
            "var o = {0:1,1:2,2:3,length:3}; o.reduce = Array.prototype.reduce; finish(o.reduce(function(a,v){return a+v;}, 10));",
            Value::Number(16.0),
        ),
        (
            "var o = {0:1,1:2,length:2}; o.find = Array.prototype.find; finish(o.find(function(v){return v>1;}));",
            Value::Number(2.0),
        ),
        (
            "var o = {0:'a',1:'b',length:2}; o.indexOf = Array.prototype.indexOf; finish(o.indexOf('b'));",
            Value::Number(1.0),
        ),
        (
            "var o = {0:5,1:9,length:2}; o.copyWithin = Array.prototype.copyWithin; o.copyWithin(1,0); finish(o[1]);",
            Value::Number(5.0),
        ),
    ] {
        assert_eq!(finished(source), expected, "{source}");
    }
}

/// A method called on `X.prototype` itself runs with the prototype object as
/// receiver: intrinsic prototypes answer, ordinary-object prototypes throw
/// the method's incompatible-receiver TypeError.
#[test]
fn prototype_methods_run_on_the_prototype_object() {
    for (source, expected) in [
        // Array.prototype is an Array; pop on it answers undefined.
        (
            "finish(String(Array.prototype.pop()));",
            Value::String("undefined".into()),
        ),
        // String.prototype is the empty String object; Number.prototype is 0.
        (
            "finish(String.prototype.toLowerCase());",
            Value::String("".into()),
        ),
        (
            "finish(Number.prototype.toFixed(1));",
            Value::String("0.0".into()),
        ),
        (
            "finish(Object.prototype.toString());",
            Value::String("[object Object]".into()),
        ),
    ] {
        assert_eq!(finished(source), expected, "{source}");
    }
    // Date.prototype and RegExp.prototype are ordinary objects — the methods
    // throw their incompatible-receiver TypeErrors.
    assert_eq!(
        finished(
            "try { Date.prototype.getTime(); finish('returned'); } catch (e) { finish(e instanceof TypeError ? e.message : 'not a TypeError'); }"
        ),
        Value::String("this is not a Date object.".into())
    );
    assert_eq!(
        finished(
            "try { RegExp.prototype.exec('x'); finish('returned'); } catch (e) { finish(e instanceof TypeError); }"
        ),
        Value::Bool(true)
    );
}

/// `Object.prototype.toString` tags the receiver's class — including the
/// `arguments` object — and `Object.prototype.valueOf` boxes primitives.
#[test]
fn builtin_tags_and_primitive_boxing() {
    for (source, expected) in [
        (
            "finish(Object.prototype.toString.call([1]));",
            Value::String("[object Array]".into()),
        ),
        (
            "finish(Object.prototype.toString.call('s') + ':' + Object.prototype.toString.call(1));",
            Value::String("[object String]:[object Number]".into()),
        ),
        (
            "finish(typeof Object.prototype.valueOf.call('s'));",
            Value::String("object".into()),
        ),
        (
            "var argObj = function() { return arguments; }(1, 2); finish(Object.prototype.toString.call(argObj));",
            Value::String("[object Arguments]".into()),
        ),
        (
            "var argObj = function() { return arguments; }(1, 2, true); finish(String.prototype.trim.call(argObj));",
            Value::String("[object Arguments]".into()),
        ),
        // An arguments object's index writes are visible to indexOf, but its
        // `length` is an ordinary property — writing past it does not extend
        // the array-like range the search scans.
        (
            "var func = function(a, b) { arguments[2] = false; return Array.prototype.indexOf.call(arguments, true) === 1 && Array.prototype.indexOf.call(arguments, false) === -1; }; finish(func(0, true));",
            Value::Bool(true),
        ),
        (
            "var argObj = function() { return arguments; }(1, 2); finish(Array.prototype.join.call(argObj, '-'));",
            Value::String("1-2".into()),
        ),
    ] {
        assert_eq!(finished(source), expected, "{source}");
    }
}

/// Holes and `length` reads on lists hold for `keys()` iteration and
/// rest-pattern destructuring too.
#[test]
fn keys_and_rest_patterns_see_the_same_list() {
    for (source, expected) in [
        // `keys()` iterates indices, including holes'.
        (
            "var a = [0, 'a', true, false, null, , undefined, NaN]; var i = 0; var out = ''; for (var v of a.keys()) { out += v + ','; i++; } finish(out + i);",
            Value::String("0,1,2,3,4,5,6,7,8".into()),
        ),
        // A rest element can destructure `length` off the collected array.
        (
            "const [...{ length }] = [1, 2, 3]; finish(length);",
            Value::Number(3.0),
        ),
        (
            "var x = null; var length = null; var result; var vals = []; result = [...{ 0: x, length }] = vals; finish(String(x) + ':' + String(length) + ':' + (result === vals));",
            Value::String("undefined:0:true".into()),
        ),
    ] {
        assert_eq!(finished(source), expected, "{source}");
    }
}
