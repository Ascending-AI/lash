//! Executable kernel laws for the built-ins' dialect adaptations (§2.4/§4).
use super::machine::{self, Ended};
use lash_kernel_doc::{Datum, Name, Value};
use lash_kernel_vm::Bindings;

fn agrees(source: &str) {
    agrees_with_bindings(source, Bindings::default());
}

fn agrees_with_bindings(source: &str, bindings: Bindings) {
    let (prefix, checks) = source
        .rsplit_once("finish(")
        .expect("a law ends with checks");
    let checks = checks.strip_suffix(");").expect("finish argument");
    let mut program = prefix.to_string();
    for check in checks.split(" && ") {
        let diagnostic = serde_json::to_string(check).expect("a check as source text");
        program.push_str(&format!("if (!({check})) {{ finish({diagnostic}); }}"));
    }
    program.push_str("finish(true);");
    assert_eq!(
        machine::end_with_bindings(&program, bindings),
        Ended::Finished(Datum::Bool(true)),
        "{source}"
    );
}

#[test]
fn map_keys_use_same_value_zero_and_explicit_object_identity() {
    agrees(
        "let invalidEntry = false; try { new Map([1]); } catch (e) { invalidEntry = e.name === 'TypeError'; } const a = {}; const b = {}; const m = new Map(); m.set(-0, 1); m.set(0, 2); m.set(NaN, 3); m.set(NaN, 4); m.set(a, 5); m.set(b, 6); m.set(undefined, 7); finish(invalidEntry && m.size === 5 && m.get(-0) === 2 && m.get(NaN) === 4 && m.get(a) === 5 && m.get(b) === 6 && m.get(undefined) === 7 && 1 / m.keys()[0] === Infinity);",
    );
}

#[test]
fn set_mutation_and_algebra_keep_insertion_order_and_zero_keys() {
    agrees(
        "const s = new Set([NaN, NaN, -0, 0, 2]); const t = s.union(new Set([3, 2])); const v = t.values(); const small = s.intersection(new Map([[2, 0], [0, 0]])); const order = small.values(); finish(s.size === 3 && s.has(NaN) && v[2] === 2 && v[3] === 3 && s.intersection(new Set([2])).size === 1 && s.difference(new Set([2])).size === 2 && s.isSubsetOf(t) && order[0] === 2 && order[1] === 0);",
    );
}

#[test]
fn map_for_each_visits_later_insertions_and_skips_deleted_keys() {
    agrees(
        "const m = new Map([[1, 10], [2, 20]]); let sum = 0; m.forEach((value, key) => { sum += value; if (key === 1) { m.delete(2); m.set(3, 30); } }); finish(sum === 40 && m.size === 2);",
    );
}

#[test]
fn function_call_apply_and_bind_preserve_receiver_and_argument_prefix() {
    agrees(
        "function f(a, b) { return this.x + a + b; } const receiver = {x: 10}; const bound = f.bind(receiver, 2); finish(f.call(receiver, 1, 2) === 13 && f.apply(receiver, [3, 4]) === 17 && bound(5) === 17);",
    );
}

#[test]
fn object_enumeration_orders_integer_names_before_insertion_order() {
    agrees(
        "let invalidEntry = false; try { Object.fromEntries([1]); } catch (e) { invalidEntry = e.name === 'TypeError'; } const o = {b: 1}; o['10'] = 10; o['2'] = 2; o.a = 3; const names = Object.keys(o); const v = Object.values(o); const e = Object.entries(o); const r = Object.fromEntries(e); const shadow = {toString: () => 'own'}; const read = shadow.toString; finish(invalidEntry && names[0] === '2' && names[1] === '10' && names[2] === 'b' && names[3] === 'a' && v[0] === 2 && r.a === 3 && Object.hasOwn(r, 'b') && r.hasOwnProperty('a') && r.toString() === '[object Object]' && shadow.toString() === 'own' && read() === 'own' && Object.is(NaN, NaN) && !Object.is(-0, 0));",
    );
}

#[test]
fn number_parsers_and_decimal_formatters_keep_javascript_edges() {
    agrees(
        "finish(Number.isNaN(parseInt()) && Number('0x10') === 16 && Number('  ') === 0 && Number.isNaN(Number('-0x1')) && parseInt(' -0x10tail') === -16 && parseFloat('1.25e2tail') === 125 && (1e20).toString() === '100000000000000000000' && (1e-6).toString() === '0.000001' && (1e21).toString() === '1e+21' && (2.55).toFixed(1) === '2.5' && (1.25).toExponential(1) === '1.3e+0' && (123.4).toPrecision(3) === '123' && (0.001).toExponential(2) === '1.00e-3' && (0.001).toPrecision(3) === '0.00100');",
    );
}

#[test]
fn number_constant_receivers_use_number_methods_and_validate_radix() {
    agrees(
        "let invalid = false; try { NaN.toString(1); } catch (e) { invalid = e instanceof RangeError; } finish(invalid && NaN.toString() === 'NaN' && Infinity.toString(16) === 'Infinity' && NaN.toExponential(101) === 'NaN' && NaN.toPrecision(0) === 'NaN' && Number.NaN.toString(10) === 'NaN');",
    );
}

#[test]
fn number_prototype_is_zero_and_borrowed_methods_require_numbers() {
    agrees(
        "let invalid = false; try { Number.prototype.valueOf.call({}); } catch (e) { invalid = e instanceof TypeError; } finish(invalid && Number.prototype.toFixed(1) === '0.0' && Number.prototype.toExponential(0) === '0e+0' && Number.prototype.toPrecision(1) === '0');",
    );
}

#[test]
fn string_normalization_orders_coercions_and_preserves_canonical_forms() {
    agrees(
        "let order = ''; const value = {toString: () => { order += 's'; return 'e\\u0301'; }}; const form = {toString: () => { order += 'f'; return 'NFC'; }}; const normalize = String.prototype.normalize; const normalized = normalize.call(value, form); let invalid = false; try { 'x'.normalize('bad'); } catch (e) { invalid = e instanceof RangeError; } let nullish = false; try { String.prototype.normalize.call(null); } catch (e) { nullish = e instanceof TypeError; } finish(order === 'sf' && normalized === 'é' && 'é'.normalize('NFD') === 'e\\u0301' && 'ﬀ'.normalize('NFKC') === 'ff' && 'ﬀ'.normalize('NFKD') === 'ff' && invalid && nullish);",
    );
}

#[test]
fn math_coercions_keep_signed_zero_nan_and_binary32_rounding() {
    agrees(
        "finish(Math.abs('-2') === 2 && Math.max(-0, 0) === 0 && 1 / Math.min(0, -0) === -Infinity && 1 / Math.round(-0.5) === -Infinity && Math.imul(4294967295, 5) === -5 && Math.imul(1.9, 2.2) === 2 && Math.clz32(1) === 31 && Math.hypot(3, 4) === 5 && Math.fround(16777217) === 16777216 && Math.fround(1e40) === Infinity && Math.PI > 3);",
    );
}

#[test]
fn errors_keep_class_cause_message_and_aliases_as_kernel_data() {
    agrees(
        "const cause = {}; const e = new TypeError('bad', {cause}); const alias = e; e.message = 'new'; const a = new AggregateError([e], 'many'); finish(e instanceof Error && e instanceof TypeError && !(e instanceof RangeError) && e.name === 'TypeError' && alias.message === 'new' && e.cause === cause && e.toString() === 'TypeError: new' && a.errors[0] === e && e !== new TypeError('new', {cause}));",
    );
}

#[test]
fn dates_use_timestamps_iso_parsing_utc_arithmetic_and_invalid_values() {
    agrees(
        "const d = new Date('1970-01-01T01:00:00+01:00'); const leap = new Date(Date.UTC(2000, 1, 29)); const invalid = new Date(NaN); finish(d.getTime() === 0 && d.toISOString() === '1970-01-01T00:00:00.000Z' && leap.getUTCFullYear() === 2000 && leap.getUTCMonth() === 1 && leap.getUTCDate() === 29 && leap.getUTCDay() === 2 && Number.isNaN(invalid.getTime()) && invalid.toJSON() === null && Date.now() === 0 && new Date(0) === new Date(0));",
    );
}

#[test]
fn url_query_mutations_keep_the_stable_live_alias_without_native_mutation() {
    agrees(
        "const u = new URL('/p?a=1&a=2', 'https://example.com'); const p = u.searchParams; p.append('b', 'hello world'); const first = u.href; u.search = '?c=3'; p.set('c', '4'); finish(first === 'https://example.com/p?a=1&a=2&b=hello+world' && p === u.searchParams && p.get('c') === '4' && u.href === 'https://example.com/p?c=4' && URL.canParse('/x', 'https://example.com') && !URL.canParse('not a url'));",
    );
}

#[test]
fn json_reviver_replacer_and_pretty_layout_keep_holder_and_omission_rules() {
    agrees(
        "const value = JSON.parse('{\"a\":1,\"b\":2}', function (key, value) { if (key === 'b') return undefined; if (key === 'a') return value + 1; return value; }); const rendered = JSON.stringify({a: NaN, b: undefined, c: [undefined, 2]}, null, 2); finish(value.a === 2 && !Object.hasOwn(value, 'b') && JSON.stringify(value) === '{\"a\":2}' && rendered === '{\\n  \"a\": null,\\n  \"c\": [\\n    null,\\n    2\\n  ]\\n}' && JSON.stringify(1, function (key, value) { return value + 1; }) === '2' && JSON.stringify(new Date(NaN)) === 'null' && JSON.stringify(new URL('https://example.com')) === '\"https://example.com/\"');",
    );
}

/// FIG-5707's hole is reserved kernel data, injected through a
/// session binding so this law can run before the Array lane lands. It exercises
/// the actual JavaScript row, including its callable value and receiver.
#[test]
fn json_sparse_slots_are_undefined_to_replacers_and_null_through_indirect_calls() {
    let mut bindings = Bindings::default();
    bindings
        .variables
        .insert(Name::new("reservedHole"), Value::Tuple(Vec::new().into()));
    agrees_with_bindings(
        "const sparse = [reservedHole, undefined]; let sawUndefined = false; let sameHolder = false; const replacer = function (key, value) { if (key === '0') { sawUndefined = value === undefined; sameHolder = this === sparse; } return value; }; const stringify = JSON.stringify; const bound = stringify.bind(null); finish(JSON.stringify(sparse) === '[null,null]' && stringify.call(null, sparse, replacer) === '[null,null]' && stringify.apply(null, [sparse]) === '[null,null]' && bound(sparse) === '[null,null]' && sawUndefined && sameHolder && JSON.stringify({omitted: undefined}) === '{}' && sparse.length === 2);",
        bindings,
    );
}
