//! Built-ins as values (FIG-5789): a built-in function, constructor,
//! namespace or prototype read as a value, and guest own-property metadata.

use super::machine::{self, Ended};
use super::remaining_builtins::agrees;
use crate::DiagnosticCode;

/// A built-in passed, held or borrowed as a value is called with the
/// receiver and arguments the program gives it.
#[test]
fn built_in_functions_are_callable_values() {
    // FIG-7451: execution alone misses admission's List/Record conflict.
    for (source, expected) in [
        (
            r#"const words = "one two three".trim().split(/\s+/).filter(Boolean); await control.finish(String(words.length));"#,
            lash_kernel_doc::Datum::Text("3".into()),
        ),
        (
            "const array = []; const object = {}; const mixed = [0, -0, '', null, undefined, NaN, false, 1, -2, '0', true, array, object]; const kept = mixed.filter(Boolean); await control.finish((kept.length === 6 && kept[0] === 1 && kept[1] === -2) && (kept[2] === '0' && kept[3] === true && kept[4] === array) && (kept[5] === object));",
            lash_kernel_doc::Datum::Bool(true),
        ),
        (
            "const values = '0 1 2'.split(/ /); const numbers = values.map(Number); const strings = numbers.map(String); const booleans = values.map(Boolean); const kept = values.filter(Number); await control.finish((numbers.join(',') === '0,1,2' && strings.join(',') === '0,1,2' && booleans.join(',') === 'true,true,true') && (kept.join(',') === '1,2' && values.every(String) && values.some(Number)) && (values.find(Number) === '1' && values.findIndex(Number) === 1));",
            lash_kernel_doc::Datum::Bool(true),
        ),
        (
            "const values = 'a b c'.split(/ /); const keep = Boolean; const map = values.map; const filter = values['filter']; const length = values['length']; const absent = values.missing; const removed = delete values.missing; await control.finish((map.call(values, String).join(',') === 'a,b,c' && filter.call(values, keep).join(',') === 'a,b,c' && length === 3) && (absent === undefined && removed && values.includes('b')) && (values.slice(1).join(',') === 'b,c'));",
            lash_kernel_doc::Datum::Bool(true),
        ),
    ] {
        let lowered =
            super::lower_with_effects(source).unwrap_or_else(|error| panic!("{source}\n{error}"));
        lash_kernel_check::derive(&lowered.document, super::library())
            .unwrap_or_else(|error| panic!("{source}\n{error}"));
        assert_eq!(machine::end(source), Ended::Finished(expected), "{source}");
    }
    agrees(
        "const numbers = ['1', '2', 'x'].map(Number); const kept = [0, 1, '', 'a'].filter(Boolean); const slice = Array.prototype.slice; function rest() { return Array.prototype.slice.call(arguments, 1); } const largest = Math.max; const atLeastTen = Math.max.bind(null, 10); await control.finish(numbers[1] === 2 && Number.isNaN(numbers[2]) && kept.length === 2 && slice.call([1, 2, 3], 1)[0] === 2 && rest(1, 2, 3).length === 2 && largest.call(null, 4, 7) === 7 && Math.max.apply(null, [1, 5, 2]) === 5 && atLeastTen(3) === 10 && Number.isNaN([3, 9].reduce(Math.max)) && [-1, 2].map(Math.abs)[0] === 1);",
    );
}

/// Every read of one built-in is the same value, which has the `name` and
/// `length` ECMA-262 gives it; namespaces and prototypes are objects.
#[test]
fn built_in_values_have_identity_name_and_length() {
    agrees(
        "const max = Math.max; const keyed = new Map([[max, 'max']]); const f = Array.prototype.slice; await control.finish([].map === Array.prototype.map && f === [1].slice && keyed.get(max) === 'max' && [...keyed.keys()][0] === Math.max && [...keyed.keys()][0](1, 2) === 2 && Math.max.name === 'max' && Math.max.length === 2 && f.name === 'slice' && f.length === 2 && Map.length === 0 && typeof Math === 'object' && typeof Array.prototype === 'object' && typeof f === 'function' && typeof Map === 'function' && Array.prototype.constructor === Array && [].constructor === Array && Object.prototype.toString.call(Math) === '[object Math]' && String(Math.abs) === 'function abs() { [native code] }');",
    );
}

/// BUILTIN-MEMBER-IDENTITY: methods that share one helper are still
/// distinct function objects. A member read answers its own prototype
/// function, with that function's name and length, while a true alias
/// (`Set.prototype.keys`) stays the one function it names.
#[test]
fn methods_sharing_a_helper_keep_their_own_identity() {
    agrees(
        "const s = 'x'; const d = new Date(0); await control.finish(s.localeCompare === String.prototype.localeCompare && s.toLocaleLowerCase === String.prototype.toLocaleLowerCase && s.toLocaleUpperCase === String.prototype.toLocaleUpperCase && s.localeCompare !== s.toLocaleUpperCase && s.toLocaleLowerCase !== s.toLocaleUpperCase && s.localeCompare.name === 'localeCompare' && s.localeCompare.length === 1 && s.toLocaleLowerCase.name === 'toLocaleLowerCase' && s.toLocaleUpperCase.length === 0 && d.toDateString === Date.prototype.toDateString && d.toDateString !== d.toTimeString && d.getTimezoneOffset.name === 'getTimezoneOffset' && d.toLocaleDateString.name === 'toLocaleDateString' && d.toTimeString.name === 'toTimeString' && new Set().keys === Set.prototype.values && new Set().keys.name === 'values');",
    );
}

/// A method read from one receiver keeps its own helper: called on a value
/// of another kind it rejects that receiver, as ECMA-262's brand checks do,
/// rather than answering with the other kind's method.
#[test]
fn a_borrowed_method_checks_the_receiver_it_is_given() {
    agrees(
        "const has = new Set([1]).has; let rejected = false; try { has.call(new Map([[1, 2]]), 1); } catch (e) { rejected = e instanceof TypeError; } await control.finish(rejected && has.call(new Set([1]), 1));",
    );
}

/// The global eval is a function value even though dynamic evaluation is refused.
#[test]
fn eval_is_a_function_value_without_dynamic_evaluation() {
    assert_eq!(
        machine::end(
            "function sameReceiver() { return this === eval; } await control.finish([1].every(sameReceiver, eval) && eval.length === 1 && eval.prototype === undefined);"
        ),
        Ended::Finished(lash_kernel_doc::Datum::Bool(true)),
    );
    assert_eq!(
        machine::end("const evaluate = eval; evaluate('1 + 2');"),
        Ended::Raised("TS_EVAL_UNSUPPORTED".to_string()),
    );
}

/// Object instances include containers and functions, but exclude primitives.
#[test]
fn object_instanceof_distinguishes_objects_from_primitives() {
    agrees(
        "let valid = true; for (const value of [{}, [], new Map(), new Set(), new Date(0), /x/, function() {}, Math, new Error()]) { valid = valid && value instanceof Object; } for (const value of [null, undefined, false, 0, 'x']) { valid = valid && !(value instanceof Object); } await control.finish(valid && !(Object.prototype instanceof Object));",
    );
}

/// Option B refuses reflective access to symbols and replacement of globals.
#[test]
fn reflective_protocols_and_builtin_global_writes_are_typed_refusals() {
    for source in [
        "Symbol.iterator;",
        "NaN = 12;",
        "undefined = 12;",
        "Object = 12;",
        "Number = 12;",
    ] {
        assert_eq!(
            super::lower(source).expect_err(source).code(),
            DiagnosticCode::ReflectionUnsupported
        );
    }
}

/// BUILTIN-INDEX: the build's immutable indexes project every owning row,
/// preserving each method's receiver order and all global/member names.
#[test]
fn builtin_indexes_project_the_owning_rows() {
    use std::collections::BTreeMap;

    use lash_ext_ts_helpers::builtins::{self, Row};
    let table = builtins::table();
    let mut methods = BTreeMap::<_, Vec<_>>::new();
    let mut properties = BTreeMap::<_, Vec<_>>::new();
    for row in builtins::objects()
        .into_iter()
        .flat_map(|object| object.rows)
    {
        match row {
            Row::Method {
                path,
                receivers,
                function,
            } => {
                assert_eq!(table.functions.get(path), Some(&function));
                let (root, _) = path.split_once('.').unwrap();
                assert!(builtins::is_global(root));
                let (_, name) = path.rsplit_once('.').unwrap();
                assert!(table.member_names.contains(name));
                methods
                    .entry(name)
                    .or_default()
                    .extend(receivers.iter().map(|receiver| (*receiver, path)));
            }
            Row::Property {
                receiver,
                name,
                function,
            } => {
                properties
                    .entry(name)
                    .or_default()
                    .push((receiver, function));
            }
            Row::Function { path, function } => {
                assert_eq!(table.functions.get(path), Some(&function));
                assert!(builtins::is_global(path.split('.').next().unwrap()));
            }
            Row::Value { path, function } => assert_eq!(table.values.get(path), Some(&function)),
            Row::Constructor { class, function } => {
                assert_eq!(table.constructors.get(class), Some(&function));
                assert!(builtins::is_global(class));
            }
            Row::InstanceOf { class, function } => {
                assert_eq!(table.instance_tests.get(class), Some(&function));
                assert!(builtins::is_global(class));
            }
        }
    }
    for (index, expected) in [(&table.methods, methods), (&table.properties, properties)] {
        let actual: BTreeMap<_, _> = index
            .iter()
            .map(|(name, rows)| (*name, rows.to_vec()))
            .collect();
        assert_eq!(actual, expected);
    }
    assert!(!builtins::is_global("UserDefined"));
    assert!(table.builtin("UserDefined").is_none());
}

/// FIG-7136: narrowing computed keys keeps numeric spellings, holes,
/// exotic indices and nullish failure order identical to the generic reader.
#[test]
fn numeric_computed_members_preserve_generic_reads() {
    let source = r#"
function read(value: any, key: number) { return value[key]; }
const record = {'0': 'zero', '-1': 'minus', '1.5': 'fraction', NaN: 'nan', Infinity: 'inf'};
const values = [record, [7, , 9], 'abc', Number, Math, new Date(0), null, undefined];
const keys = [0, -0, -1, 1.5, 999, NaN, Infinity, -Infinity];
let out = [];
for (const value of values) {
  for (const key of keys) {
    try { const result = read(value, key); out.push(Number.isNaN(result) ? 'nan' : result); }
    catch (error) { out.push(error.name); }
  }
}
await control.finish(out);
"#;
    let generic = source.replace("key: number", "key: any");
    let numeric = machine::end(source);
    assert_eq!(numeric, machine::end(&generic));
    let Ended::Finished(lash_kernel_doc::Datum::List(results)) = numeric else {
        panic!("the witness must finish every read");
    };
    assert_eq!(results.len(), 64);
}

/// L11: guest own metadata agrees for direct, aliased and computed paths.
#[test]
fn builtin_own_metadata_is_independent_of_access_path() {
    let source = r#"
const math = Math;
const name = 'max';
const descriptor = Object.getOwnPropertyDescriptor(math, name);
const names = Object.getOwnPropertyNames(math);
const re = /x/g;
const last = Object.getOwnPropertyDescriptor(re, 'lastIndex');
const proto = Object.getOwnPropertyDescriptor(Array, 'prototype');
const getter = Object.getOwnPropertyDescriptor(Map.prototype, 'size');
let conversions = 0;
const missingKey = {toString() { conversions++; return 'missing'; }};
const missing = missingKey in math;
const reflectedMissing = Reflect.has(math, missingKey);
function authored(a, b) { return a + b; }
await control.finish(
  name in math && Object.hasOwn(math, name) && !math.propertyIsEnumerable(name) &&
  math[name] === Math.max && descriptor.value === Math.max && descriptor.writable &&
  !descriptor.enumerable && descriptor.configurable && names.includes(name) &&
  Object.getOwnPropertySymbols(math).length === 0 &&
  Object.getOwnPropertyDescriptor(authored, 'length').value === 2 &&
  !Object.getOwnPropertyDescriptor(authored, 'name').writable &&
  Object.hasOwn(re, 'lastIndex') && !re.propertyIsEnumerable('lastIndex') &&
  last.value === 0 && last.writable && !last.enumerable && !last.configurable &&
  proto.value === Array.prototype && !proto.writable && !proto.configurable &&
  JSON.stringify(Array.prototype) === '[]' && String(Array.prototype) === '' &&
  JSON.stringify(Number.prototype) === '0' && Number(Number.prototype) === 0 &&
  Reflect.get(math, name) === Math.max && Reflect.has(math, name) &&
  Reflect.get(Math.max, 'constructor') === Function &&
  Reflect.get(new Map([['x', 1]]), 'size') === 1 &&
  Reflect.get(new Map(), 'size', new Map([['x', 1]])) === 1 &&
  Reflect.getOwnPropertyDescriptor(math, name).value === descriptor.value &&
  Reflect.ownKeys(re).join(',') === 'lastIndex' &&
  !missing && !reflectedMissing && conversions === 2 &&
  Number.prototype.valueOf() === 0 && Number.prototype.toString() === '0' &&
  String.prototype.valueOf() === '' && Boolean.prototype.valueOf() === false &&
  Boolean.prototype.toString() === 'false' &&
  Object.getOwnPropertyNames(Math.max).join(',') === 'length,name' &&
  Object.getOwnPropertyDescriptors(math).max.value === Math.max &&
  RegExp.prototype.source === '(?:)' && RegExp.prototype['flags'] === '' &&
  Reflect.get(RegExp.prototype, 'source') === RegExp.prototype.source &&
  Object.getOwnPropertyNames(new URL('https://example.test/')).length === 0 &&
  Object.keys(re).length === 0 &&
  getter.get.call(new Map([['x', 1]])) === 1 && getter.set === undefined &&
  !Object.hasOwn(getter, 'writable') && !getter.enumerable && getter.configurable
);
"#;
    agrees(&source.trim().replace("&&\n", "&& "));
}

/// L07: constructor tokens retain identity through aliases and computed reads;
/// callee and arguments run once in source order.
#[test]
fn builtin_constructor_aliases_keep_their_identity() {
    agrees(r#"
const namespace = globalThis;
const key = 'Array';
const C = namespace[key];
const R = RegExp;
const O = Object;
const D: (...args: any[]) => any = Array;
let trace = '';
function constructor() { trace += 'c'; return Map; }
function entries() { trace += 'a'; return [['x', 7]]; }
const mapped = new (constructor())(entries());
const array = new C(2);
const regex = new R('x', 'g');
await control.finish([] instanceof D && new D(1).length === 1 && C === Array && array instanceof C && regex instanceof R &&
  mapped instanceof Map && mapped.get('x') === 7 && trace === 'ca' &&
  array instanceof O && new O() instanceof O && namespace.Math === Math && namespace.globalThis === namespace &&
  namespace.process === undefined && namespace['fetch'] === undefined);
"#.trim());
}

/// TR22 native/bound callables expose native spelling, never captures.
#[test]
fn native_callable_strings_do_not_expose_captures() {
    agrees(
        "const secret = {private: 7}; function authored() { return secret; } const bound = authored.bind(null); await control.finish(bound.toString() === 'function () { [native code] }' && Math.abs.toString() === 'function abs() { [native code] }' && String(bound) === bound.toString());",
    );
}

/// L12: mutators write the existing items list and preserve alias identity
/// and optional match metadata, including after holes and length changes.
#[test]
fn match_array_mutations_preserve_aliases_and_metadata() {
    agrees(r#"
const match = /a(.)/.exec('ab');
const alias = match;
const index = match.index;
const input = match.input;
const groups = match.groups;
const receivers = match.map((value, index, receiver) => receiver === match);
function append(items: string[]) {
  let joined = '';
  for (const item of items) { joined += item; }
  const readable = items[0] === 'ab' && items.length === 2 && items.slice(0, 1)[0] === 'ab' &&
    items.map((value, index, receiver) => receiver === items).every(Boolean) && joined === 'abb';
  items.push('c');
  return readable;
}
const typed = append(match);
match.reverse();
match.splice(1, 1, 'z');
delete match[1];
Object.assign(match, {'1': 'b'});
const removed = match.pop();
Object.assign(match, {length: 1});
const regex = /x/g;
Object.assign(regex, {lastIndex: 3});
const url = new URL('https://example.test/a');
Object.assign(url, {pathname: '/b'});
const array = [];
Object.assign(array, {'2': 9});
await control.finish(typed && receivers.every(Boolean) && alias === match && match[0] === 'c' && removed === 'ab' &&
  match.length === 1 && match.index === index && match.input === input &&
  match.groups === groups && regex.lastIndex === 3 && url.pathname === '/b' &&
  array.length === 3 && !(0 in array) && array[2] === 9);
"#.trim());
}
