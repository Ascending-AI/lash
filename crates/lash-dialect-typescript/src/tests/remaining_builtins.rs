//! Executable kernel laws for the built-ins' dialect adaptations (§2.4/§4).
use super::machine::{self, Ended};
use super::pages::Session;
use lash_kernel_doc::{Datum, Name};

/// BRAND-ONE: native and body constructors agree on one canonical brand
/// and name the artifact that built the value, including nested URL params.
#[test]
fn brand_one_package_constructors_use_canonical_brands_and_artifact_digests() {
    let cases = [
        (
            "regex.ecma",
            "reg_exp",
            "let value = regex.ecma.compile_check(\"a\", \"\")",
        ),
        (
            "date.ecma",
            "invalid_date",
            "let value = date.ecma.timestamp(nan)",
        ),
        (
            "url.whatwg",
            "url",
            "let value = url.whatwg.parse(\"https://example.test/?a=1\", absent)",
        ),
        (
            "url.whatwg",
            "url_search_params",
            "let url = url.whatwg.parse(\"https://example.test/?a=1\", absent) let value = url.searchParams",
        ),
        (
            "url.whatwg",
            "url_search_params",
            "let value = invoke ts.url_search_params.construct(absent, [])",
        ),
        (
            "typescript",
            "promise",
            "let value = invoke ts.promise.pending()",
        ),
        (
            "typescript",
            "arguments",
            "let value = invoke ts.arguments([])",
        ),
        (
            "typescript",
            "iterator",
            "let value = invoke ts.array.keys([], [])",
        ),
        (
            "typescript",
            "iterator",
            "let value = invoke ts.array.values([], [])",
        ),
        (
            "typescript",
            "iterator",
            "let value = invoke ts.array.entries([], [])",
        ),
        (
            "typescript",
            "regex_match",
            "let value = invoke ts.regexp.match_record({index: 0, groups: [\"a\"], named: null}, \"a\")",
        ),
    ];
    let registry = machine::registry();
    let mut header = String::from("kernel 1\nnumbers float\n");
    for (id, function) in registry.iter() {
        header.push_str(&format!("use {} = @{id}\n", function.name()));
    }
    for (package, ty, constructor) in cases {
        let source = format!(
            "{header}entry run() -> Any\nfn run() {{\n{constructor}\n\
             let brand = invoke brand.of(value)\n\
             let built = invoke {package}.__built()\n\
             return (brand, value.__lash_built, built)\n}}\nmain {{}}\n"
        );
        let mut document = lash_kernel_doc::parse_document(&source).expect("constructor law");
        document.manifest.functions =
            lash_kernel_check::requirements(&document, registry.as_ref()).functions;
        let (_, result) = machine::end_of_entry(document, "run", vec![]);
        let Datum::Tuple(parts) = result else {
            panic!("constructor returns its brand and digests: {package}.{ty}");
        };
        assert_eq!(
            parts[0],
            Datum::Tuple(vec![Datum::Text(package.into()), Datum::Text(ty.into())]),
            "{constructor}"
        );
        assert_eq!(parts[1], parts[2], "{constructor}");
        assert!(
            matches!(&parts[1], Datum::Bytes(bytes) if bytes.as_slice().len() == 32),
            "{constructor}"
        );
    }
}

pub(super) fn agrees(source: &str) {
    agrees_with_bindings(source, super::pages::Session::default());
}

fn agrees_with_bindings(source: &str, bindings: Session) {
    let (prefix, checks) = source
        .rsplit_once("await finish(")
        .expect("a law ends with checks");
    let checks = checks.strip_suffix(");").expect("finish argument");
    let mut program = prefix.to_string();
    for check in checks.split(" && ") {
        let diagnostic = serde_json::to_string(check).expect("a check as source text");
        program.push_str(&format!(
            "if (!({check})) {{ await finish({diagnostic}); }}"
        ));
    }
    program.push_str("await finish(true);");
    assert_eq!(
        machine::end_with_bindings(&program, bindings),
        Ended::Finished(Datum::Bool(true)),
        "{source}"
    );
}

/// Map checks NewTarget before touching the optional iterable.
#[test]
fn map_calls_require_a_new_target_before_reading_the_iterable() {
    agrees(
        "let count = 0; try { Map(); } catch (e) { if (e.name === 'TypeError') count++; } try { Map({toString() { throw 1; }}); } catch (e) { if (e.name === 'TypeError') count++; } await finish(count === 2 && new Map().size === 0);",
    );
}

/// Set checks NewTarget before touching the optional iterable.
#[test]
fn set_calls_require_a_new_target_before_reading_the_iterable() {
    agrees(
        "let count = 0; try { Set(); } catch (e) { if (e.name === 'TypeError') count++; } try { Set({toString() { throw 1; }}); } catch (e) { if (e.name === 'TypeError') count++; } await finish(count === 2 && new Set().size === 0);",
    );
}

#[test]
fn map_keys_use_same_value_zero_and_explicit_object_identity() {
    agrees(
        "let invalidEntry = false; try { new Map([1]); } catch (e) { invalidEntry = e.name === 'TypeError'; } const a = {}; const b = {}; const m = new Map(); m.set(-0, 1); m.set(0, 2); m.set(NaN, 3); m.set(NaN, 4); m.set(a, 5); m.set(b, 6); m.set(undefined, 7); await finish(invalidEntry && m.size === 5 && m.get(-0) === 2 && m.get(NaN) === 4 && m.get(a) === 5 && m.get(b) === 6 && m.get(undefined) === 7 && 1 / m.keys()[0] === Infinity);",
    );
}

/// ECMA-262 Map methods default missing arguments only after evaluating
/// every supplied argument, and borrowed methods reject the receiver first.
#[test]
fn map_methods_preserve_missing_arguments_receiver_errors_and_evaluation_order() {
    agrees(
        r#"const m = new Map(); let order = ''; function mark(tag, value) { order += tag; return value; } m.set(); const missing = m.has() && m.get() === undefined; m.set(undefined, 9); const replaced = m.get() === 9 && m.size === 1; m.set(1); const valueMissing = m.has(1) && m.get(1) === undefined; const key = {}; m.set(mark('k', key), mark('v', 7), mark('x', 8)); const extraGet = m.get(mark('g', key), mark('y', 0)); const has = m.has(mark('h', key), mark('z', 0)); const removed = m.delete(mark('d', key), mark('w', 0)); let rejected = 0; for (const method of [Map.prototype.get, Map.prototype.set, Map.prototype.has, Map.prototype.delete]) { try { method.call({}, mark('a', {}), mark('b', 1)); } catch (e) { if (e.name === 'TypeError') rejected++; } } const unknown = {get: function(value) { return this.value + value; }, value: 4}; const absentRemoved = m.delete(); await finish(missing && replaced && valueMissing && extraGet === 7 && has && removed && order === 'kvxgyhzdwabababab' && rejected === 4 && unknown.get(3) === 7 && absentRemoved && !m.has());"#,
    );
}

/// ECMA-262 SameValueZero: mutation cannot change an object's identity key;
/// functions and errors retain their identities when the keys are enumerated.
#[test]
fn map_identity_keys_survive_mutation_and_key_roundtrips() {
    agrees(
        r#"const a = {}; const b = {}; const list = []; const map = new Map(); const set = new Set(); const f = () => 1; const g = () => 1; const e = new Error('x'); const other = new Error('x'); const m = new Map([[a, 1], [b, 2], [list, 3], [map, 4], [set, 5], [f, 6], [g, 7], [e, 8], [other, 9], [undefined, 10], [-0, 11], [NaN, 12]]); a.x = 99; list.push(99); map.set(99, 99); set.add(99); const keys = m.keys(); await finish(m.size === 12 && m.get(a) === 1 && m.get(b) === 2 && m.get(list) === 3 && m.get(map) === 4 && m.get(set) === 5 && m.get(f) === 6 && m.get(g) === 7 && m.get(e) === 8 && m.get(other) === 9 && keys[0] === a && keys[2] === list && keys[5] === f && keys[7] === e && keys[9] === undefined && Object.is(keys[10], 0) && Number.isNaN(keys[11]) && m.has(-0) && m.has(NaN));"#,
    );
}

#[test]
fn set_mutation_and_algebra_keep_insertion_order_and_zero_keys() {
    agrees(
        "const s = new Set([NaN, NaN, -0, 0, 2]); const t = s.union(new Set([3, 2])); const v = t.values(); const small = s.intersection(new Map([[2, 0], [0, 0]])); const order = small.values(); await finish(s.size === 3 && s.has(NaN) && v[2] === 2 && v[3] === 3 && s.intersection(new Set([2])).size === 1 && s.difference(new Set([2])).size === 2 && s.isSubsetOf(t) && order[0] === 2 && order[1] === 0);",
    );
}

#[test]
fn map_for_each_visits_later_insertions_and_skips_deleted_keys() {
    agrees(
        "const m = new Map([[1, 10], [2, 20]]); let sum = 0; m.forEach((value, key) => { sum += value; if (key === 1) { m.delete(2); m.set(3, 30); } }); await finish(sum === 40 && m.size === 2);",
    );
}

#[test]
fn function_call_apply_and_bind_preserve_receiver_and_argument_prefix() {
    agrees(
        "function f(a, b) { return this.x + a + b; } const receiver = {x: 10}; const bound = f.bind(receiver, 2); await finish(f.call(receiver, 1, 2) === 13 && f.apply(receiver, [3, 4]) === 17 && bound(5) === 17);",
    );
}

/// ECMA-262 OrdinaryOwnPropertyKeys: canonical array indexes precede other
/// strings, which retain insertion order, in each enumeration consumer.
#[test]
fn object_enumeration_orders_integer_names_before_insertion_order() {
    agrees(
        "let invalidEntry = false; try { Object.fromEntries([1]); } catch (e) { invalidEntry = e.name === 'TypeError'; } const o = {b: 1}; o['10'] = 10; o['2'] = 2; o.a = 3; const names = Object.keys(o); const v = Object.values(o); const e = Object.entries(o); const r = Object.fromEntries(e); const shadow = {toString: () => 'own'}; const read = shadow.toString; await finish(invalidEntry && names[0] === '2' && names[1] === '10' && names[2] === 'b' && names[3] === 'a' && v[0] === 2 && r.a === 3 && Object.hasOwn(r, 'b') && r.hasOwnProperty('a') && r.toString() === '[object Object]' && shadow.toString() === 'own' && read() === 'own' && Object.is(NaN, NaN) && !Object.is(-0, 0));",
    );
    agrees(
        r#"const o = {}; o.b = 'b'; o['4294967295'] = 'max'; o['10'] = 'ten'; o['01'] = 'leading'; o['2'] = 'two'; o['-0'] = 'negative'; o['1e0'] = 'exponent'; o['4294967294'] = 'last'; o['0'] = 'zero'; o[''] = 'empty'; o.a = 'a'; const names = ['0', '2', '10', '4294967294', 'b', '4294967295', '01', '-0', '1e0', '', 'a']; const values = ['zero', 'two', 'ten', 'last', 'b', 'max', 'leading', 'negative', 'exponent', 'empty', 'a']; const keys = Object.keys(o); const entries = Object.entries(o); let ordered = true; for (let i = 0; i < names.length; i++) { if (keys[i] !== names[i] || entries[i][0] !== names[i] || entries[i][1] !== values[i]) ordered = false; } await finish(ordered && keys.length === names.length && JSON.stringify(Object.values(o)) === JSON.stringify(values) && JSON.stringify(o) === '{"0":"zero","2":"two","10":"ten","4294967294":"last","b":"b","4294967295":"max","01":"leading","-0":"negative","1e0":"exponent","":"empty","a":"a"}');"#,
    );
}

/// Object prototype methods coerce keys before checking the receiver, keep
/// non-enumerable length distinct from indices, and invoke an own toString.
#[test]
fn object_prototype_methods_keep_coercion_enumerability_and_receiver() {
    agrees(
        "let coerced = false; let rejected = false; const key = {toString() { coerced = true; return 'x'; }}; try { Object.prototype.propertyIsEnumerable.call(null, key); } catch (e) { rejected = e.name === 'TypeError'; } function getArgs() { return arguments; } const argumentObject = getArgs(1); const a = [1, , 3]; const o = {x: 1, toString() { return this.x; }}; let prototypeRejected = false; try { Object.prototype.isPrototypeOf.call(null, {}); } catch (e) { prototypeRejected = e.name === 'TypeError'; } await finish(argumentObject.propertyIsEnumerable('0') && !argumentObject.propertyIsEnumerable('length') && !argumentObject.propertyIsEnumerable('callee') && coerced && rejected && a.propertyIsEnumerable('0') && !a.propertyIsEnumerable('1') && !a.propertyIsEnumerable('length') && o.toLocaleString() === 1 && Object.prototype.isPrototypeOf.call(null, 1) === false && prototypeRejected);",
    );
    agrees(
        "function getArgs() { arguments.extra = 4; return arguments; } const args = getArgs(1, 2); const sparse = [1, , 3]; await finish(JSON.stringify(Object.keys(args)) === '[\"0\",\"1\",\"extra\"]' && JSON.stringify(Object.values(args)) === '[1,2,4]' && JSON.stringify(Object.keys(sparse)) === '[\"0\",\"2\"]' && JSON.stringify(Object.values(sparse)) === '[1,3]');",
    );
}

/// Enumeration boxes a string conceptually without exposing a wrapper value.
#[test]
fn object_string_enumeration_uses_utf16_indices_and_assign_copies_them() {
    agrees(
        "const keys = Object.keys('abc'); const values = Object.values('abc'); const entries = Object.entries('abc'); const assigned = Object.assign({}, 'ab'); await finish(keys.length === 3 && keys[0] === '0' && keys[2] === '2' && values[1] === 'b' && entries[2][1] === 'c' && assigned[1] === 'b' && Object.keys('😀').length === 2);",
    );
}

/// Option B's primitive wrappers are refused by name, including indirect
/// Object conversion and the generic Object.prototype.valueOf path.
#[test]
fn object_primitive_wrappers_are_typed_refusals() {
    assert_eq!(
        machine::end("const wrap = Object; await finish(wrap(true));"),
        Ended::Raised("TS_BOXED_PRIMITIVE_UNSUPPORTED".into())
    );
    assert_eq!(
        machine::end("await finish(Object.prototype.valueOf.call(false));"),
        Ended::Raised("TS_BOXED_PRIMITIVE_UNSUPPORTED".into())
    );
}

/// Strict user and bound functions inherit the caller/arguments poison.
#[test]
fn user_functions_and_bind_poison_restricted_properties() {
    agrees(
        "function f(a, b = 2, ...rest) { return a + b; } const bound = f.bind(null, 3); let count = 0; try { f.caller; } catch (e) { if (e.name === 'TypeError') count++; } try { bound.arguments; } catch (e) { if (e.name === 'TypeError') count++; } try { bound.caller = {}; } catch (e) { if (e.name === 'TypeError') count++; } await finish(count === 3 && bound() === 5 && !bound.hasOwnProperty('caller'));",
    );
}

#[test]
fn number_parsers_and_decimal_formatters_keep_ecmascript_edges() {
    agrees(
        "await finish(Number.isNaN(parseInt()) && Number('0x10') === 16 && Number('  ') === 0 && Number.isNaN(Number('-0x1')) && parseInt(' -0x10tail') === -16 && parseFloat('1.25e2tail') === 125 && (1e20).toString() === '100000000000000000000' && (1e-6).toString() === '0.000001' && (1e21).toString() === '1e+21' && (2.55).toFixed(1) === '2.5' && (1.25).toExponential(1) === '1.3e+0' && (123.4).toPrecision(3) === '123' && (0.001).toExponential(2) === '1.00e-3' && (0.001).toPrecision(3) === '0.00100');",
    );
}

/// ECMA Number::toString and precision formatting keep their separate
/// layouts, signed-zero rules and exact halfway rounding.
#[test]
fn decimal_parts_keep_javascript_layout_and_precision_rounding() {
    agrees(
        "await finish(String(-0) === '0' && String(NaN) === 'NaN' && String(Infinity) === 'Infinity' && String(-Infinity) === '-Infinity' && String(5e-324) === '5e-324' && String(2.2250738585072014e-308) === '2.2250738585072014e-308' && String(1.7976931348623157e308) === '1.7976931348623157e+308' && String(0.1 + 0.2) === '0.30000000000000004' && String(1e-7) === '1e-7' && String(1e-6) === '0.000001' && String(1e20) === '100000000000000000000' && String(1e21) === '1e+21' && (-0).toExponential() === '0e+0' && (-0).toPrecision(2) === '0.0' && (1.25).toFixed(1) === '1.3' && (2.55).toFixed(1) === '2.5' && (1.25).toExponential(1) === '1.3e+0' && (9.99).toPrecision(2) === '10' && (0.001).toPrecision(3) === '0.00100');",
    );
}

#[test]
fn number_constant_receivers_use_number_methods_and_validate_radix() {
    agrees(
        "let invalid = false; try { NaN.toString(1); } catch (e) { invalid = e instanceof RangeError; } await finish(invalid && NaN.toString() === 'NaN' && Infinity.toString(16) === 'Infinity' && NaN.toExponential(101) === 'NaN' && NaN.toPrecision(0) === 'NaN' && Number.NaN.toString(10) === 'NaN');",
    );
}

#[test]
fn number_prototype_is_zero_and_borrowed_methods_require_numbers() {
    agrees(
        "let invalid = false; try { Number.prototype.valueOf.call({}); } catch (e) { invalid = e instanceof TypeError; } await finish(invalid && Number.prototype.toFixed(1) === '0.0' && Number.prototype.toExponential(0) === '0e+0' && Number.prototype.toPrecision(1) === '0');",
    );
}

#[test]
fn string_normalization_orders_coercions_and_preserves_canonical_forms() {
    agrees(
        "let order = ''; const value = {toString: () => { order += 's'; return 'e\\u0301'; }}; const form = {toString: () => { order += 'f'; return 'NFC'; }}; const normalize = String.prototype.normalize; const normalized = normalize.call(value, form); let invalid = false; try { 'x'.normalize('bad'); } catch (e) { invalid = e instanceof RangeError; } let nullish = false; try { String.prototype.normalize.call(null); } catch (e) { nullish = e instanceof TypeError; } await finish(order === 'sf' && normalized === 'é' && 'é'.normalize('NFD') === 'e\\u0301' && 'ﬀ'.normalize('NFKC') === 'ff' && 'ﬀ'.normalize('NFKD') === 'ff' && invalid && nullish);",
    );
}

#[test]
fn math_coercions_keep_signed_zero_nan_and_binary32_rounding() {
    agrees(
        "await finish(Math.abs('-2') === 2 && Math.max(-0, 0) === 0 && 1 / Math.min(0, -0) === -Infinity && 1 / Math.round(-0.5) === -Infinity && Math.imul(4294967295, 5) === -5 && Math.imul(1.9, 2.2) === 2 && Math.clz32(1) === 31 && Math.hypot(3, 4) === 5 && Math.fround(16777217) === 16777216 && Math.fround(1e40) === Infinity && Math.PI > 3);",
    );
}

#[test]
fn errors_keep_class_cause_message_and_aliases_as_kernel_data() {
    agrees(
        "const cause = {}; const e = new TypeError('bad', {cause}); const alias = e; e.message = 'new'; const a = new AggregateError([e], 'many'); await finish(e instanceof Error && e instanceof TypeError && !(e instanceof RangeError) && e.name === 'TypeError' && alias.message === 'new' && e.cause === cause && e.toString() === 'TypeError: new' && a.errors[0] === e && e !== new TypeError('new', {cause}));",
    );
}

/// Error instance slots are writable and configurable but not enumerable;
/// inherited name is not an own slot until assigned, and absent message stays absent.
#[test]
fn error_instance_own_slots_keep_presence_attributes_and_deletion() {
    agrees(
        "const cause = {}; const e = new Error('bad', {cause}); const a = new AggregateError([]); const empty = new Error(); let rejected; try { await Promise.any([Promise.reject(7)]); } catch (error) { rejected = error; } let noMembers; try { await Promise.any([]); } catch (error) { noMembers = error; } e.extra = 3; let own = Object.hasOwn(e, 'cause'); let present = 'cause' in e; let inherited = 'name' in e; let named = Object.hasOwn(e, 'name'); const keys = Object.keys(e); e.cause = 4; delete e.message; delete e.cause; await finish(own && present && inherited && !named && keys.length === 1 && keys[0] === 'extra' && !Object.hasOwn(e, 'message') && !Object.hasOwn(e, 'cause') && e.message === '' && !Object.hasOwn(empty, 'message') && Object.hasOwn(a, 'errors') && !a.propertyIsEnumerable('errors') && rejected.errors[0] === 7 && Object.hasOwn(rejected, 'message') && noMembers.errors.length === 0 && !noMembers.propertyIsEnumerable('message'));",
    );
    agrees(
        r#"const e = new Error('hidden', {cause: 5}); e.b = 'b'; e['10'] = 'ten'; e['01'] = 'leading'; e['2'] = 'two'; e['4294967295'] = 'max'; e['-0'] = 'negative'; e['1e0'] = 'exponent'; e['4294967294'] = 'last'; e['0'] = 'zero'; e.a = 'a'; await finish(JSON.stringify(Object.keys(e)) === '["0","2","10","4294967294","b","01","4294967295","-0","1e0","a"]' && JSON.stringify(Object.values(e)) === '["zero","two","ten","last","b","leading","max","negative","exponent","a"]' && JSON.stringify(Object.entries(e)[0]) === '["0","zero"]');"#,
    );
}

#[test]
fn dates_use_timestamps_iso_parsing_utc_arithmetic_and_invalid_values() {
    agrees(
        "const d = new Date('1970-01-01T01:00:00+01:00'); const leap = new Date(Date.UTC(2000, 1, 29)); const invalid = new Date(NaN); await finish(d.getTime() === 0 && d.toISOString() === '1970-01-01T00:00:00.000Z' && leap.getUTCFullYear() === 2000 && leap.getUTCMonth() === 1 && leap.getUTCDate() === 29 && leap.getUTCDay() === 2 && Number.isNaN(invalid.getTime()) && invalid.toJSON() === null && Date.now() === 0 && new Date(0) === new Date(0));",
    );
}

#[test]
fn url_query_mutations_keep_the_stable_live_alias_without_native_mutation() {
    agrees(
        "const u = new URL('/p?a=1&a=2', 'https://example.com'); const p = u.searchParams; p.append('b', 'hello world'); const first = u.href; u.search = '?c=3'; p.set('c', '4'); await finish(first === 'https://example.com/p?a=1&a=2&b=hello+world' && p === u.searchParams && p.get('c') === '4' && u.href === 'https://example.com/p?c=4' && URL.canParse('/x', 'https://example.com') && !URL.canParse('not a url'));",
    );
}

#[test]
fn json_reviver_replacer_and_pretty_layout_keep_holder_and_omission_rules() {
    agrees(
        "const value = JSON.parse('{\"a\":1,\"b\":2}', function (key, value) { if (key === 'b') return undefined; if (key === 'a') return value + 1; return value; }); const rendered = JSON.stringify({a: NaN, b: undefined, c: [undefined, 2]}, null, 2); await finish(value.a === 2 && !Object.hasOwn(value, 'b') && JSON.stringify(value) === '{\"a\":2}' && rendered === '{\\n  \"a\": null,\\n  \"c\": [\\n    null,\\n    2\\n  ]\\n}' && JSON.stringify(1, function (key, value) { return value + 1; }) === '2' && JSON.stringify(new Date(NaN)) === 'null' && JSON.stringify(new URL('https://example.com')) === '\"https://example.com/\"');",
    );
}

/// FIG-5707's hole is reserved kernel data, injected through a
/// session binding so this law can run before the Array lane lands. It exercises
/// the actual JavaScript row, including its callable value and receiver.
#[test]
fn json_sparse_slots_are_undefined_to_replacers_and_null_through_indirect_calls() {
    let bindings = Session::seeded([(Name::new("reservedHole"), Datum::Tuple(Vec::new()))]);
    agrees_with_bindings(
        "const sparse = [reservedHole, undefined]; let sawUndefined = false; let sameHolder = false; const replacer = function (key, value) { if (key === '0') { sawUndefined = value === undefined; sameHolder = this === sparse; } return value; }; const stringify = JSON.stringify; const bound = stringify.bind(null); await finish(JSON.stringify(sparse) === '[null,null]' && stringify.call(null, sparse, replacer) === '[null,null]' && stringify.apply(null, [sparse]) === '[null,null]' && bound(sparse) === '[null,null]' && sawUndefined && sameHolder && JSON.stringify({omitted: undefined}) === '{}' && sparse.length === 2);",
        bindings,
    );
}

/// K-LJSON-004/005 and the console dialect rule: strict data, insertion
/// order, guest number spelling, shared subgraphs and atomic printing.
#[test]
fn console_json_is_strict_and_keeps_guest_spelling_and_print_order() {
    let recorded = machine::run(
        r#"
        const shared = {n: -0, fraction: 1.25, small: 1e-7, big: 1e21};
        const holes = [, undefined, 2];
        console.log({z: shared, a: shared, holes: holes, quoted: '"\n'});
        console.log({nested: {brand: "opaque", n: 2}});
        console.log({brand: 2, n: 3});
        let first = "";
        try { console.log("unprinted", {a: undefined, b: NaN}); } catch (error) { first = error.name; }
        let second = "";
        try { console.log({a: NaN, b: undefined}); } catch (error) { second = error.name; }
        console.log(first, second, !(0 in holes), holes[1] === undefined);
        const numbers = [0, -0, 1e-6, 1e-7, 1e20, 1e21, 5e-324, Number.MAX_VALUE,
                         1000000000000000100, 1.0000000000000002, 9007199254740992, -1.25];
        console.log(numbers);
        console.log(JSON.stringify(numbers));
        "#,
        &[],
    );
    assert_eq!(recorded.end, "ok");
    let lines = recorded.lines();
    assert_eq!(
        &lines[..4],
        [
            r#"{"z":{"n":0,"fraction":1.25,"small":1e-7,"big":1e+21},"a":{"n":0,"fraction":1.25,"small":1e-7,"big":1e+21},"holes":[null,null,2],"quoted":"\"\n"}"#,
            r#"{"nested":{"brand":"opaque","n":2}}"#,
            r#"{"brand":2,"n":3}"#,
            "not_data json_number true true",
        ]
    );
    // The direct console writer and the generic guest JSON walk share the
    // dialect's spelling even at shortest-conversion and notation edges.
    assert_eq!(lines.len(), 6);
    assert_eq!(lines[4], lines[5]);
    assert_eq!(
        lines[4],
        "[0,0,0.000001,1e-7,100000000000000000000,1e+21,5e-324,1.7976931348623157e+308,1000000000000000100,1.0000000000000002,9007199254740992,-1.25]"
    );
}

/// FIG-5903: a text brand is ordinary data, including JSON and tool records.
/// The expected text follows Node's property order and JSON number spelling.
#[test]
fn text_brand_fields_remain_ordinary_guest_data() {
    agrees(
        "const r = {brand: 'shoe', __lash_brand: () => 0}; const marker = [1].values().__lash_brand; const fake = {brand: 'Promise', __lash_brand: marker}; let dateRejected = false; try { Date.prototype.valueOf.call({brand: 'date.invalid'}); } catch (e) { dateRejected = e.name === 'TypeError'; } await finish(r.brand === 'shoe' && !(r instanceof Promise) && marker === undefined && fake.brand === 'Promise' && !(fake instanceof Promise) && dateRejected);",
    );

    let source = r#"
        const parsed = JSON.parse('{"brand":"Nike","price":3}');
        if (parsed.brand !== 'Nike' || parsed.price !== 3) {
            await finish(JSON.stringify([parsed.brand, parsed.price, Object.keys(parsed), JSON.stringify(parsed)]));
        }
        const literal = {brand: 'shoe', size: 42};
        const copied = {...literal};
        const fake = {brand: 'Promise', state: 'fulfilled', task: null};
        const marker = {__lash_brand: ['Promise'], brand: 'shoe', size: 42};
        const rows = [parsed, literal, copied, toolRecord, marker];
        const result = [];
        for (const r of rows) {
            result.push([r.brand, r.price, r.size, Object.keys(r), JSON.stringify(r)]);
        }
        result.push(fake instanceof Promise, (await fake) === fake);
        literal.brand = 'boot'; delete literal.size;
        result.push(literal.brand, Object.keys(literal));
        await finish(JSON.stringify(result));
    "#;
    let bindings = Session::seeded([(
        Name::new("toolRecord"),
        Datum::Record(vec![
            ("brand".into(), Datum::Text("Nike".into())),
            (
                "price".into(),
                Datum::Float(lash_kernel_doc::Float::new(3.0)),
            ),
        ]),
    )]);
    assert_eq!(
        machine::end_with_bindings(source, bindings),
        Ended::Finished(Datum::Text(r#"[["Nike",3,null,["brand","price"],"{\"brand\":\"Nike\",\"price\":3}"],["shoe",null,42,["brand","size"],"{\"brand\":\"shoe\",\"size\":42}"],["shoe",null,42,["brand","size"],"{\"brand\":\"shoe\",\"size\":42}"],["Nike",3,null,["brand","price"],"{\"brand\":\"Nike\",\"price\":3}"],["shoe",null,42,["__lash_brand","brand","size"],"{\"__lash_brand\":[\"Promise\"],\"brand\":\"shoe\",\"size\":42}"],false,true,"boot",["brand"]]"#.into()))
    );
}
