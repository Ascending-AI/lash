//! FIG-3787: function call, apply and bind, and re-attached built-in
//! methods, run with ECMA receivers.

use super::*;

/// `call`, `apply`, and `bind` run a guest function with the given receiver
/// and argument list; `bind` composes, prepending its bound arguments.
#[test]
fn call_apply_and_bind_run_guest_functions() {
    for (source, expected) in [
        (
            "var f = function(a, b) { return a + b; }; finish(f.call(null, 1, 2));",
            Value::Number(3.0),
        ),
        (
            "var f = function(a, b) { return a + b; }; finish(f.apply(null, [1, 2]));",
            Value::Number(3.0),
        ),
        // `apply` takes any array-like, not only a List.
        (
            "var f = function(a, b) { return a + b; }; finish(f.apply(null, {0:'x',1:'y',length:2}));",
            Value::String("xy".into()),
        ),
        (
            "var f = function(a, b) { return a + b; }; var g = f.bind(null, 1); finish(g(2));",
            Value::Number(3.0),
        ),
        // A bound chain prepends at each link: `f.bind(null,1).bind(null,9)`
        // calls `f(1, 9, ...)`.
        (
            "var f = function(a,b){return a*10+b;}; var g = f.bind(null,1).bind(null,9); finish(g(2));",
            Value::Number(19.0),
        ),
        // A bound `this` survives later binds and calls.
        (
            "var f = function(a){return this.k + a;}; var g = f.bind({k:10}, 1).bind(null, 9); finish(g(2));",
            Value::Number(11.0),
        ),
        // `call` itself is a function on Function.prototype.
        (
            "finish(Function.prototype.call.call(function(a){return a+1;}, null, 4));",
            Value::Number(5.0),
        ),
    ] {
        assert_eq!(finished(source), expected, "{source}");
    }
}

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

/// `call`, `apply`, and `bind` work on built-in methods too — the same
/// mechanism as guest functions.
#[test]
fn builtin_methods_compose_through_call_apply_and_bind() {
    for (source, expected) in [
        (
            "var o = {0:'q',length:1}; finish(Array.prototype.pop.call(o) + ':' + o.length);",
            Value::String("q:0".into()),
        ),
        (
            "var o = {length:0}; Array.prototype.push.apply(o, ['a','b']); finish(o.length + ':' + o[1]);",
            Value::String("2:b".into()),
        ),
        (
            "var bound = Array.prototype.join.bind({0:'a',1:'b',length:2}, '-'); finish(bound());",
            Value::String("a-b".into()),
        ),
        (
            "var o = {length:0}; o.push = Array.prototype.push; var g = o.push.bind(o, 'z'); g(); finish(o.length + ':' + o[0]);",
            Value::String("1:z".into()),
        ),
    ] {
        assert_eq!(finished(source), expected, "{source}");
    }
    // A null/undefined receiver is a TypeError, as Node answers.
    assert_eq!(
        finished(
            "try { Array.prototype.pop.call(null); finish('returned'); } catch (e) { finish(e instanceof TypeError); }"
        ),
        Value::Bool(true)
    );
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

/// A callback's `thisArg` is the receiver the callback sees, and iteration
/// re-reads the receiver: a delete before the visit is observed, an append
/// past the captured length is not.
#[test]
fn callbacks_see_thisarg_and_live_mutation() {
    for (source, expected) in [
        (
            "var o = {0:'x',1:'y',length:2}; var seen=[]; o.forEach = Array.prototype.forEach; o.forEach(function(v){ seen.push(this.p + v); }, {p:'>'}); finish(seen.join(','));",
            Value::String(">x,>y".into()),
        ),
        (
            "var a=[0,1,2,3]; var seen=''; a.forEach(function(v,k,arr){seen+=v;if(k===0)delete arr[1];}); finish(seen);",
            Value::String("023".into()),
        ),
        (
            "var a=[1,2,3,4,5]; var calls=0; a.forEach(function(v,k,o){calls++; if(k===0)delete o[3];}); finish(calls);",
            Value::Number(4.0),
        ),
        (
            "var a=[0,1,2]; var seen=''; a.forEach(function(v,k,arr){seen+=v;if(k===0)arr[3]=9;}); finish(seen);",
            Value::String("012".into()),
        ),
        // reduce deletes inside the window skip those indices.
        (
            "var arr=['1',2,3,4,5]; var s=arr.reduce(function(a,v,k,o){ if(k===1){delete o[3];delete o[4];} return a+v;}); finish(s);",
            Value::String("123".into()),
        ),
        // sort takes a comparator callback and writes back by position.
        (
            "var a = [3,1,2]; a.sort(function(x,y){return x-y;}); finish(a.join(','));",
            Value::String("1,2,3".into()),
        ),
    ] {
        assert_eq!(finished(source), expected, "{source}");
    }
}

/// Holes are holes: `in`/`hasOwnProperty` see them, `map` preserves them,
/// `indexOf`/`lastIndexOf` skip them while `includes` reads them as
/// `undefined`, and `copyWithin` moves absence itself.
#[test]
fn sparse_receivers_have_real_holes() {
    for (source, expected) in [
        (
            "var a = [1,,3]; var r = a.map(function(v){return v*2;}); finish(r.length + ':' + r[0] + ':' + r[2] + ':' + (1 in r));",
            Value::String("3:2:6:false".into()),
        ),
        (
            "var a=[1,,3]; finish((0 in a)+':'+(1 in a)+':'+(2 in a));",
            Value::String("true:false:true".into()),
        ),
        (
            "var a=[1,,3]; delete a[0]; finish(a.hasOwnProperty(0)+':'+a.hasOwnProperty(2));",
            Value::String("false:true".into()),
        ),
        (
            "var arr=[0,,2]; finish(arr.indexOf(undefined));",
            Value::Number(-1.0),
        ),
        (
            "var arr=[0,,2]; finish(arr.lastIndexOf(undefined));",
            Value::Number(-1.0),
        ),
        (
            "var arr=[0,,2]; finish(arr.includes(undefined));",
            Value::Bool(true),
        ),
        (
            "var arr=[0,1,,,1]; arr.copyWithin(0,1,4); finish(arr.hasOwnProperty(1)+':'+arr[0]+':'+arr[4]);",
            Value::String("false:1:1".into()),
        ),
    ] {
        assert_eq!(finished(source), expected, "{source}");
    }
}

/// A huge array-like length does not iterate absent indices, and `splice`
/// moves canonical integer-index keys at the 2**53 - 1 boundary.
#[test]
fn huge_array_like_lengths_do_not_iterate_absent_indices() {
    for (source, expected) in [
        (
            "var o={length:4294967296,4294967295:'z'}; var c=0; Array.prototype.forEach.call(o,function(){c++;}); finish(c);",
            Value::Number(1.0),
        ),
        (
            "var al = {}; al['9007199254740986']='A'; al['9007199254740987']='B'; al['9007199254740988']='C'; al['9007199254740990']='D'; al.length=9007199254740993; var r=Array.prototype.splice.call(al, 9007199254740987, 1); finish(r.join(',') + ':' + al.length + ':' + al['9007199254740987'] + ':' + ('9007199254740988' in al) + ':' + al['9007199254740989']);",
            Value::String("B:9007199254740990:C:false:D".into()),
        ),
    ] {
        assert_eq!(finished(source), expected, "{source}");
    }
}

/// `Array.of` and `Array.from` honor the spec's receiver rules: a
/// non-constructor receiver falls back to a plain Array.
#[test]
fn array_statics_take_the_receiver() {
    for (source, expected) in [
        (
            "finish(Array.of.call(undefined, 1, 2).join(','));",
            Value::String("1,2".into()),
        ),
        (
            "finish(Array.from.call(undefined, {0:'a',length:1}).join(','));",
            Value::String("a".into()),
        ),
        (
            "finish(Array.from({0:'x',2:'z',length:3}).join(','));",
            Value::String("x,,z".into()),
        ),
        (
            "finish(Array.from({0:1,1:2,length:2}, function(v, i){return v*10+i;}).join(','));",
            Value::String("10,21".into()),
        ),
        (
            "try { Array.from(null); finish('returned'); } catch (e) { finish(e instanceof TypeError); }",
            Value::Bool(true),
        ),
    ] {
        assert_eq!(finished(source), expected, "{source}");
    }
}

/// `Date.prototype.toJSON` is generic: it `ToPrimitive`s the receiver, then
/// invokes `toISOString` on whatever object that produced.
#[test]
fn date_tojson_invokes_the_receiver() {
    for (source, expected) in [
        (
            "var r = {}; finish(Date.prototype.toJSON.call({toISOString:function(){return r;}}) === r);",
            Value::Bool(true),
        ),
        // valueOf runs before toISOString; its non-finite answer short-circuits.
        (
            "var calls=''; var d={valueOf:function(){calls+='v';return 0;},toISOString:function(){calls+='i';return 'ISO';}}; finish(Date.prototype.toJSON.call(d)+':'+calls);",
            Value::String("ISO:vi".into()),
        ),
        (
            "finish(String(Date.prototype.toJSON.call({valueOf:function(){return NaN;}})));",
            Value::String("null".into()),
        ),
    ] {
        assert_eq!(finished(source), expected, "{source}");
    }
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

/// Arguments to a detached built-in convert through the guest's `valueOf` and
/// `toString` hooks, and a missing hook set is a TypeError — the same
/// coercion machinery a member call replays.
#[test]
fn detached_arguments_convert_through_guest_hooks() {
    for (source, expected) in [
        (
            "var o = {valueOf:function(){return 4;}}; finish(String.prototype.padStart.call('ab', o, 'q'));",
            Value::String("qqab".into()),
        ),
        (
            "finish(String.prototype.padStart.call('ab', 4, 'q'));",
            Value::String("qqab".into()),
        ),
        (
            "try { Number.prototype.toFixed.call(1.5, {valueOf:undefined,toString:undefined}); finish('returned'); } catch (e) { finish(e instanceof TypeError); }",
            Value::Bool(true),
        ),
        // Function.prototype.toString requires a callable receiver.
        (
            "try { Function.prototype.toString.call(42); finish('returned'); } catch (e) { finish(e instanceof TypeError); }",
            Value::Bool(true),
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
