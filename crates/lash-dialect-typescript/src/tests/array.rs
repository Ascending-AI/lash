//! JavaScript array laws: sparse slots, length snapshots, live iteration and
//! callbacks run on the real kernel library, never a native stand-in.

use std::sync::{Arc, OnceLock};

use lash_kernel_dialect::{Environment, NamedLibrary};
use lash_kernel_doc::Datum;

use lash_kernel_vm::{
    Bounds, CellStart, End, Host, Machine, Outcome, PreparedLibrary, Request, Step, Target,
};

struct Kernel {
    library: NamedLibrary,
    prepared: PreparedLibrary,
}

fn kernel() -> &'static Kernel {
    static KERNEL: OnceLock<Kernel> = OnceLock::new();
    KERNEL.get_or_init(|| {
        let mut registry = super::kernel_registry();
        let mut library = NamedLibrary::from_registry(&registry).unwrap();
        for definition in lash_ext_ts_helpers::package::define_helpers(&mut library).unwrap() {
            registry.register(definition, None).unwrap();
        }
        Kernel {
            library,
            prepared: PreparedLibrary::with_helpers(
                Arc::new(registry.clone()),
                Arc::new(
                    lash_kernel_doc::BuildHelpers::new(
                        registry.clone(),
                        registry.iter().map(|(id, _)| *id),
                    )
                    .expect("closed helpers"),
                ),
            ),
        }
    })
}

#[derive(Default)]
struct NoHost;

impl Host for NoHost {
    fn print(&mut self, _value: &Datum) {}
    fn cancel_requested(&mut self) -> bool {
        false
    }
}

fn run(source: &str) -> End {
    drive(source, &[])
}

fn drive(source: &str, expected: &[Vec<Datum>]) -> End {
    let kernel = kernel();
    let environment_namespaces =
        crate::tests::linked::namespaces(&super::machine::effects(), super::controls())
            .expect("the test's effects are operations");
    let environment = Environment::new(&kernel.library, &environment_namespaces);
    let lowered = crate::lower(
        source,
        &environment,
        lash_kernel_dialect::Mode::Cell(lash_kernel_dialect::CellEnvironment::first()),
    )
    .unwrap_or_else(|error| panic!("{error}\n{source}"));
    drive_document(lowered.document.into_document(), expected, source)
}

fn drive_document(
    document: lash_kernel_doc::Document,
    expected: &[Vec<Datum>],
    source: &str,
) -> End {
    let kernel = kernel();
    let text = lash_kernel_doc::print_document(&document);
    let program = crate::tests::linked::program(Arc::new(document), kernel.prepared.clone());
    let bounds = Bounds {
        memory_meter: lash_kernel_vm::MemoryMeter::On,
        charge: lash_kernel_vm::Charge::Exact,
        charge_limit: 100_000_000,
        memory: 16 << 20,
        call_depth: 200,
        live_tasks: 100,
        requests_per_park: 100,
        join_members: 100,
    };
    let mut machine = super::pages::start(
        program,
        bounds,
        CellStart {
            target: Target::Main,
            args: Vec::new(),
            session: None,
            owner: super::pages::OWNER,
            modes: Default::default(),
            host: Vec::new(),
        },
    )
    .unwrap();
    let mut asked = 0;
    // The value the cell's `finish` call ended it with: the run's answer.
    let mut finished_with = None;
    loop {
        match machine.run(&mut NoHost, u64::MAX).unwrap() {
            Step::Ended(mut end) => {
                assert_eq!(asked, expected.len(), "{source}");
                if let (End::Finished(finished), Some(value)) = (&mut end, finished_with) {
                    finished.result = value;
                }
                return end;
            }
            Step::Parked(park) => {
                for request in park.requests {
                    match request {
                        Request::Effect(effect)
                            if crate::tests::linked::operation(&effect.use_).as_str()
                                == "control.finish" =>
                        {
                            finished_with =
                                Some(effect.args.first().cloned().unwrap_or(Datum::Null));
                            machine
                                .deliver(effect.wait, Outcome::Completed(Datum::Null))
                                .unwrap();
                        }
                        Request::Effect(effect)
                            if super::linked::operation(&effect.use_).as_str()
                                == "typescript.sleep" =>
                        {
                            machine
                                .deliver(effect.wait, Outcome::Completed(Datum::Null))
                                .unwrap();
                        }
                        Request::Effect(effect) => {
                            assert_eq!(effect.args, expected[asked], "{source}");
                            asked += 1;
                            machine
                                .deliver(effect.wait, Outcome::Completed(Datum::Null))
                                .unwrap();
                        }
                    }
                }
            }
            other => panic!("{other:?}\n{source}\n{text}"),
        }
    }
}

fn law(source: &str) {
    match run(source) {
        End::Finished(finished) => assert_eq!(finished.result, Datum::Bool(true), "{source}"),
        other => panic!("{other:?}\n{source}"),
    }
}

/// Expression consumers and spread arguments still get push's new length.
#[test]
fn consumed_and_spread_pushes_return_the_new_length() {
    law(r#"
        const a = [];
        const n = a.push(1);
        const f = (x) => x;
        const passed = f(a.push(2, 3));
        const expression = [a.push(4) + 1];
        const spread = a.push(...[5, 6]);
        a.push(...[7, 8]);
        const empty = a.push();
        function typed(a: number[]) { return a.push(9); }
        await control.finish(n === 1 && passed === 3 && expression[0] === 5
            && spread === 6 && empty === 8 && typed(a) === 9
            && a.join(',') === '1,2,3,4,5,6,7,8,9');
    "#);
}

/// Receiver evaluation precedes arguments; every argument precedes the
/// outer append, whose positions use the list's length after argument mutation.
#[test]
fn discarded_multi_argument_push_preserves_evaluation_and_mutation_order() {
    law(r#"
        const a = ['start']; let trace = '';
        const receiver = () => { trace += 'receiver;'; return a; };
        const first = () => { trace += 'first:' + a.length + ';'; a.push('inner'); return 'one'; };
        const second = () => { trace += 'second:' + a.join(',') + ';'; return 'two'; };
        receiver().push(first(), second());
        function typed(a: string[]) { a.push('typed-one', 'typed-two'); }
        typed(a);
        let caught = false;
        const fail = () => { throw 'stop'; };
        try { a.push('never', fail()); } catch (e) { caught = e === 'stop'; }
        await control.finish(trace === 'receiver;first:1;second:start,inner;'
            && a.join(',') === 'start,inner,one,two,typed-one,typed-two' && caught);
    "#);
}

/// A non-array method is read before its arguments, keeps its receiver,
/// and is called even when its result is discarded. Borrowed push stays generic.
#[test]
fn discarded_push_on_non_arrays_keeps_generic_method_semantics() {
    law(r#"
        let trace = '';
        const o = { tag: 'old', push(x, y) { trace += this.tag + ':' + x + y + ';'; return 99; } };
        const replace = () => { o.push = () => { trace += 'replacement;'; return 7; }; return 'x'; };
        o.push(replace(), 'y');
        o.push();
        const arraylike = { length: 1, 0: 'a', push: Array.prototype.push };
        arraylike.push('b', 'c');
        const unknown = (value) => { value.push('d'); };
        unknown(arraylike);
        const a = [];
        a.push(o.push());
        await control.finish(trace === 'old:xy;replacement;replacement;'
            && arraylike.length === 4 && arraylike[1] === 'b'
            && arraylike[2] === 'c' && arraylike[3] === 'd' && a[0] === 7);
    "#);
}

/// ECMA Array's absent slots differ from present undefined values, and an
/// index write/length write preserve the same array identity.
#[test]
fn holes_length_and_canonical_index_keys_keep_array_identity() {
    law(r#"
        const a = [1, , undefined]; const alias = a;
        delete a[0]; a[4] = 8; a.length = 6;
        let present = ''; for (const key in a) present += key + ',';
        const before = a.length === 6 && !(0 in a) && !(1 in a) && (2 in a)
            && !(3 in a) && (4 in a) && !(5 in a) && a[1] === undefined
            && a['01'] === undefined && present === '2,4,';
        a.length = 3;
        await control.finish(before && alias === a && alias.length === 3 && !(4 in a));
    "#);
}

/// Callback methods take their length once, test live properties, pass the
/// original receiver and thisArg, and preserve holes in map's result.
#[test]
fn callbacks_snapshot_length_but_read_live_properties() {
    law(r#"
        const a = [1, , 3]; const context = {offset: 10}; let indices = '';
        const mapped = a.map(function(value, index, receiver) {
            indices += index;
            if (index === 0) { a[1] = 2; a.push(4); }
            return this.offset + value + (receiver === a ? 0 : 100);
        }, context);
        const sparse = [1, , 3].map(x => x * 2);
        await control.finish(indices === '012' && mapped.join(',') === '11,12,13'
            && sparse.length === 3 && !(1 in sparse) && sparse[2] === 6);
    "#);
    law(r#"
        const a = [undefined, 2, 3, 4]; let indices = ''; const calls = [];
        const mapped = a.map(function(value, index, receiver) {
            indices += index; calls.push(arguments);
            if (index === 0) { delete a[1]; a.length = 3; a.push(5, 6); }
            if (index === 2) { a.length = 3; }
            return value;
        });
        const like = {0: 1, 2: 3, length: 3};
        const generic = Array.prototype.map.call(like, function(value, index, receiver) {
            if (index === 0) { like[1] = 2; delete like[2]; }
            return value + (receiver === like ? 0 : 100);
        });
        const slots = indices === '02' && mapped.length === 4 && (0 in mapped)
            && mapped[0] === undefined && !(1 in mapped) && mapped[2] === 3
            && !(3 in mapped);
        const escaped = calls.length === 2 && calls[0] !== calls[1]
            && calls[0][1] === 0 && calls[1][1] === 2 && calls[0][2] === a;
        const fallback = generic.length === 3 && generic[0] === 1 && generic[1] === 2
            && !(2 in generic);
        await control.finish(slots && escaped && fallback);
    "#);
}

/// A split with a limit builds only the pieces it returns: a list of a
/// million pieces, at 16 bytes a piece, would not fit this run's 16 MiB
/// (FIG-5825).
#[test]
fn a_limited_split_builds_only_the_pieces_it_returns() {
    law(r#"
        const commas = ",".repeat(1000000);
        const first = commas.split(",", 1);
        const two = commas.split("", 2);
        await control.finish(first.length === 1 && first[0] === "" && two.join("|") === ",|,"
            && "a😀b".split("", 1).join("|") === "a");
    "#);
}

/// Async callbacks are ordinary calls: each starts a task immediately, map
/// retains the resulting promises and the aggregate joins them concurrently.
#[test]
fn async_map_callbacks_start_concurrently_and_join() {
    law(r#"
        let order = '';
        const promises = [1, 2, 3].map(async function(value) {
            order += value; await sleep(1); order += value; return value * 2;
        });
        const started = order === '123';
        const result = await Promise.all(promises);
        await control.finish(started && result.join(',') === '2,4,6' && order === '123123');
    "#);
}

/// Default sorting compares UTF-16 units, is stable, and puts undefined before
/// holes. A NaN callback comparison is equal, unlike the strict kernel sort.
#[test]
fn sort_uses_utf16_stability_and_undefined_before_holes() {
    law(r#"
        const a = ['\uE000', , undefined, '😀', 'a']; const alias = a;
        a.sort();
        const ordered = a[0] === 'a' && a[1] === '😀' && a[2] === '\uE000'
            && (3 in a) && a[3] === undefined && !(4 in a);
        const records = [{key: 1, id: 'a'}, {key: 1, id: 'b'}, {key: 0, id: 'c'}];
        records.sort((x, y) => x.key - y.key);
        const equal = [2, 1].sort(() => NaN);
        await control.finish(ordered && alias === a && records.map(x => x.id).join('') === 'cab'
            && equal.join(',') === '2,1');
    "#);
}

/// FIG-6948: a default sort takes both strings at each comparison, as
/// SortCompare does, wherever an element's ToString may be observed: an
/// object's `toString` runs twice per comparison and not at all for a
/// single element, and one that throws leaves the array as it was. Where
/// every element is a primitive, the strings order by UTF-16 code units,
/// stably.
#[test]
fn default_sort_takes_an_objects_string_at_each_comparison() {
    law(r#"
        let made = 0;
        const spell = (v) => ({ v, toString() { made++; return String(v); } });
        const xs = [spell(10), spell(9), spell(1), spell(100)];
        xs.sort();
        const order = xs.map(x => x.v).join() === '1,10,100,9';
        const each = made >= 6 && made % 2 === 0;
        made = 0;
        [spell(7)].sort();
        const none = made === 0;
        const ys = [spell(2), 1, { toString() { throw new Error('no'); } }];
        let thrown = false;
        try { ys.sort(); } catch (e) { thrown = e.message === 'no'; }
        const kept = ys[0].v === 2 && ys[1] === 1 && ys.length === 3;
        const zs = [10, 'b', 9, true, null, 'B', 1, '\uE000', '\uD83D\uDE00', 'b'].sort();
        const units = zs.map(String).join('|') === '1|10|9|B|b|b|null|true|\uD83D\uDE00|\uE000';
        await control.finish(order && each && none && thrown && kept && units);
    "#);
}

/// FIG-6886: a comparator sort applies its comparator exactly where a
/// bottom-up merge of runs 1, 2, 4… compares, to the same (left, right)
/// pairs in the same order: at most n⌈log2 n⌉ applications, stably.
#[test]
fn comparator_sort_applies_the_comparator_once_per_merge_comparison() {
    law(r#"
        const n = 100; const xs = []; let seed = 6886;
        for (let i = 0; i < n; i++) { seed = (seed * 1103 + 12345) % 65536; xs.push({ id: i, key: seed % 17 }); }
        const expected = []; let runs = xs.slice(); let width = 1;
        while (width < n) {
            const out = [];
            for (let start = 0; start < n; start += 2 * width) {
                const mid = Math.min(start + width, n); const end = Math.min(mid + width, n);
                let left = start; let right = mid;
                while (left < mid && right < end) {
                    expected.push(runs[left].id + ':' + runs[right].id);
                    if (runs[left].key - runs[right].key > 0) { out.push(runs[right]); right++; }
                    else { out.push(runs[left]); left++; }
                }
                while (left < mid) { out.push(runs[left]); left++; }
                while (right < end) { out.push(runs[right]); right++; }
            }
            runs = out; width = width * 2;
        }
        const calls = [];
        xs.sort((p, q) => { calls.push(p.id + ':' + q.id); return p.key - q.key; });
        let stable = true;
        for (let i = 1; i < n; i++) {
            if (xs[i - 1].key > xs[i].key || (xs[i - 1].key === xs[i].key && xs[i - 1].id > xs[i].id)) { stable = false; }
        }
        await control.finish(stable && calls.length <= n * 7 && calls.join(',') === expected.join(',')
            && runs.map(r => r.id).join(',') === xs.map(r => r.id).join(','));
    "#);
}

/// Splice, reverse and overlapping copyWithin preserve property presence,
/// while change-by-copy methods materialize holes and keep the source intact.
#[test]
fn mutation_preserves_holes_and_copy_methods_materialize_them() {
    law(r#"
        const a = [1, , 3, 4]; const removed = a.splice(1, 2, 'x');
        const sparseRemoved = removed.length === 2 && !(0 in removed) && removed[1] === 3;
        a.copyWithin(1, 0, 2); a.reverse();
        const sparse = [1, , 3]; const reversed = sparse.toReversed();
        const changed = sparse.with(-2, 9);
        const sorted = sparse.toSorted();
        await control.finish(sparseRemoved && a.join(',') === 'x,1,1'
            && (1 in reversed) && reversed[1] === undefined && !(1 in sparse)
            && changed.join(',') === '1,9,3' && sorted[2] === undefined && (2 in sorted));
    "#);
}

/// Iteration reads live by index, visits holes as undefined, and spread copies
/// its iterable before evaluating a later element.
#[test]
fn iteration_is_live_and_spread_finishes_before_later_operands() {
    law(r#"
        const a = [1, , 3]; let visited = '';
        for (const item of a) { visited += item === undefined ? 'u' : item; if (item === 1) a.push(4); }
        const spread = [...a, a.push(5)];
        const iterator = a.values(); const first = iterator.next();
        a[1] = 2; const second = iterator.next();
        const values = [...iterator];
        await control.finish(visited === '1u34' && spread.length === 5 && spread[4] === 5
            && (1 in spread) && first.value === 1 && second.value === 2
            && values.join(',') === '3,4,5');
    "#);
}

/// Generic Array methods use ToLength and indexed properties rather than
/// requiring a list; fractional and negative bounds use ToIntegerOrInfinity.
#[test]
fn generic_receivers_bounds_search_and_reduce_follow_ecma_steps() {
    law(r#"
        const record = {0: 'a', 2: 'c', length: 3.9};
        const sliced = Array.prototype.slice.call(record, -2.8);
        const a = [1, , 3, 4];
        const reduced = a.reduce((sum, value) => sum + value, 0);
        const reverse = a.reduceRight((text, value) => text + value, '');
        const sparse = sliced.length === 2 && !(0 in sliced) && sliced[1] === 'c';
        const holes = a.includes(undefined) && a.indexOf(undefined) === -1;
        const nans = [NaN].includes(NaN) && [NaN].indexOf(NaN) === -1;
        const bounds = a.slice(-2.9).join(',') === '3,4';
        const shortened = [1, 2];
        const from = {valueOf() { shortened.length = 0; return 0; }};
        const shrink = shortened.indexOf(undefined, from) === -1;
        await control.finish(sparse && holes && nans && bounds && shrink && reduced === 8 && reverse === '431');
    "#);
    law(r#"
        const inner = {value: 1}; const a = [inner, , undefined, 4]; let order = '';
        const start = {valueOf() { order += 's'; a.length = 2; return -3.9; }};
        const stop = {valueOf() { order += 'e'; a[1] = inner; return Infinity; }};
        const sliced = a.slice(start, stop);
        sliced[0].value = 9;
        const sparse = [1, , undefined].slice(1);
        const grown = [1, 2];
        const growth = {valueOf() { grown.push(3); return -1; }};
        const snapshot = grown.slice(growth);
        const shrunk = order === 'se' && sliced.length === 3 && sliced[0] === inner
            && inner.value === 9 && !(1 in sliced) && !(2 in sliced);
        const slots = sparse.length === 2 && !(0 in sparse) && (1 in sparse)
            && sparse[1] === undefined;
        const empty = a.slice(Infinity).length === 0 && a.slice(1, -Infinity).length === 0
            && a.slice(NaN, -0).length === 0;
        await control.finish(shrunk && slots && empty && snapshot.join(',') === '2');
    "#);
}

/// A boolean or number receiver is the wrapper object ToObject makes for
/// the call, so a generic method's writes to it succeed and are lost.
#[test]
fn generic_methods_write_to_a_primitive_receivers_wrapper() {
    law(r#"
        const pushed = Array.prototype.push.call(true) === 0 && Array.prototype.push.call(1, 'x') === 1;
        const popped = Array.prototype.pop.call(false) === undefined && Array.prototype.shift.call(2) === undefined;
        const spliced = Array.prototype.splice.call(true).length === 0 && Array.prototype.unshift.call(false) === 0;
        await control.finish(pushed && popped && spliced);
    "#);
}

/// Array.from interleaves iterator reads with mapping, whereas an array-like
/// source snapshots ToLength. Flattening skips holes at every flattened level.
#[test]
fn from_maps_between_live_iterator_steps_and_flatten_skips_holes() {
    law(r#"
        const source = [1, 2];
        const mapped = Array.from(source, function(value, index) {
            if (index === 0) source.push(3);
            return value + this.offset;
        }, {offset: 10});
        const record = {0: 1, length: 1};
        const snapshot = Array.from(record, function(value) { record.length = 2; record[1] = 2; return value; });
        const flat = [1, , [2, , [3]]].flat(2);
        const flatMapped = [1, , 2].flatMap(x => [x, , x * 2]);
        const first = mapped.join(',') === '11,12,13' && snapshot.join(',') === '1';
        await control.finish(first && flat.join(',') === '1,2,3' && flatMapped.join(',') === '1,2,2,4');
    "#);
}

/// Invalid lengths raise RangeError, and a typed read of a present index
/// containing a hole gives undefined rather than the internal marker.
#[test]
fn invalid_lengths_raise_and_typed_sparse_reads_hide_holes() {
    for length in ["-1", "1.5", "Infinity", "4294967296"] {
        let source =
            format!("const a = [1, , 3]; a.length = {length}; await control.finish(true);");
        assert!(
            matches!(run(&source), End::Error(lash_kernel_vm::RunError::Uncaught { error: Datum::Error(error), .. }) if error.kind == "RangeError"),
            "{source}"
        );
    }
    law(
        "const a: number[] = [1, , 3]; await control.finish(a[1] === undefined && a.length === 3);",
    );
}

/// The final result exposes JSON-shaped array slots, including nested aliases,
/// while a boundary copy leaves the guest's sparse property presence intact.
#[test]
fn final_results_normalize_holes_without_mutating_guest_arrays() {
    let end =
        run("const a = [1, , undefined]; const copied = [a, a]; await control.finish(copied);");
    let row = Datum::List(vec![
        Datum::Float(lash_kernel_doc::Float::new(1.0)),
        Datum::Null,
        Datum::Null,
    ]);
    assert!(
        matches!(end, End::Finished(finished) if finished.result == Datum::List(vec![row.clone(), row]))
    );
}

/// Both a spawned tool and an in-place perform send a copied graph with null
/// array slots. Delivery and later mutation still see the original holes.
#[test]
fn tool_and_effect_arguments_normalize_nested_holes_on_copies() {
    let row = Datum::List(vec![
        Datum::Null,
        Datum::Float(lash_kernel_doc::Float::new(2.0)),
    ]);
    let expected = [
        vec![row.clone()],
        vec![Datum::Record(vec![("nested".into(), row)])],
    ];
    let end = drive(
        r#"
        const a = [, 2]; const p = host.echo(a); await p;
        await host.echo({nested: a});
        await control.finish(!(0 in a) && a.length === 2 && a[1] === 2);
    "#,
        &expected,
    );
    assert!(matches!(end, End::Finished(finished) if finished.result == Datum::Bool(true)));
}

/// K-LJSON-001/005: an opaque subtree inherits both the current depth and
/// the active ancestors of console's strict renderer.
#[test]
fn console_subtrees_keep_depth_and_cycle_failure_order() {
    let kernel = kernel();
    let uses = kernel
        .library
        .iter()
        .map(|(name, _)| format!("use {name}\n"))
        .collect::<String>();
    let text = format!(
        r#"{uses}
function law.main() -> Any
kernel 1
metering statements
body {{
  let leaf = map{{"n": 2.0}}
  let value = leaf
  let i = 0
  while num.lt(i, 63) {{ set value = [value] set i = num.add(i, 1) }}
  let accepted = invoke ts.console.json(value)
  let overflow = [value]
  let depth = ""
  try {{ let ignored = invoke ts.console.json(overflow) }} catch invalid {{ set depth = invalid.kind }}
  let root = {{}}
  let opaque = map{{"root": root}}
  set root.opaque = opaque
  let cycle = ""
  try {{ let ignored = invoke ts.console.json(root) }} catch invalid {{ set cycle = invalid.kind }}
  finish [accepted, depth, cycle]
}}
"#
    );
    let document = super::linked_inspection::main(&text, &kernel.library);
    let end = drive_document(document, &[], &text);
    assert!(
        matches!(&end, End::Finished(finished) if finished.result == Datum::List(vec![
            Datum::Text(format!("{}{}{}", "[".repeat(63), r#"{"n":2.0}"#, "]".repeat(63))),
            Datum::Text("json_depth".into()),
            Datum::Text("cycle".into()),
        ])),
        "{end:?}"
    );
}

/// Named properties are still outside the dialect, including keys produced
/// by negative/fractional indices and the non-index uint32 upper bound.
#[test]
fn non_index_array_writes_keep_the_existing_typed_refusal() {
    for key in ["-1", "1.5", "4294967295", "'extra'"] {
        let source = format!("const a = [1]; a[{key}] = 9; await control.finish(true);");
        assert!(
            matches!(run(&source), End::Error(lash_kernel_vm::RunError::Uncaught { error: Datum::Error(error), .. }) if error.kind == "TS_ARRAY_NON_INDEX_PROPERTY_UNSUPPORTED"),
            "{source}"
        );
    }
}

/// TS_TYPED_NUMBER_ADD checks the value read after an untyped alias writes
/// through the same array; it does not coerce a text into a number.
#[test]
fn typed_array_alias_writes_are_checked_at_the_typed_use() {
    let source = "const xs: number[] = [1, 2]; const alias: any = xs; alias[0] = 'a'; await control.finish(xs[0] + 1);";
    assert!(
        matches!(run(source), End::Error(lash_kernel_vm::RunError::Uncaught { error: Datum::Error(error), .. }) if error.kind == "type_error")
    );
    law(
        "const xs: any = [1, 2]; const alias: any = xs; alias[0] = 'a'; await control.finish(xs[0] + 1 === 'a1');",
    );
}

/// Declared aliases, interfaces, function results and for-of elements retain
/// their types while the iterator takes live steps over an array.
#[test]
fn live_for_of_carries_declared_element_and_return_types() {
    let source = "type Price = number; interface Item { price: Price } function total(items: Item[]): number { let sum = 0; for (const item of items) { sum += item.price; } return sum; }";
    law(&format!(
        "{source} await control.finish(total([{{price: 2}}, {{price: 3}}]) * 2 === 10);"
    ));
    assert!(
        matches!(run(&format!("{source} await control.finish(total([{{price: '2' as any}}]));")), End::Error(lash_kernel_vm::RunError::Uncaught { error: Datum::Error(error), .. }) if error.kind == "type_error")
    );
}

/// Array string conversion invokes a generic receiver's own join.
#[test]
fn string_conversion_obeys_own_join() {
    law(
        "const a = {0: 1, length: 1, join: function() { return this[0] + 10; }}; await control.finish(Array.prototype.toString.call(a) === 11 && Array.prototype.toString.call({length: 1}) === '[object Object]');",
    );
}

/// ECMA ArrayCreate refuses a length above 2^32 - 1, so slice and the copy
/// methods raise RangeError on a huge array-like before they read it, and
/// toSpliced reads only the elements its result keeps. Each used to walk
/// 2^32 or 2^53 - 1 indices until the memory bound (FIG-5788).
#[test]
fn huge_array_likes_are_refused_before_a_walk_and_to_spliced_reads_what_it_keeps() {
    for call in [
        "Array.prototype.slice.call(huge, 0, 4294967296)",
        "Array.prototype.toReversed.call(huge)",
        "Array.prototype.toSorted.call(huge)",
        "Array.prototype.with.call(huge, 0, 1)",
        "Array.prototype.toSpliced.call(huge, 0, 0)",
    ] {
        let source = format!(
            "const huge: any = {{}}; huge[0] = 'x'; huge[4294967295] = 'y'; huge.length = 4294967296; {call}; await control.finish(true);"
        );
        assert!(
            matches!(run(&source), End::Error(lash_kernel_vm::RunError::Uncaught { error: Datum::Error(error), .. }) if error.kind == "RangeError"),
            "{source}"
        );
    }
    law(
        "const like = {'9007199254740989': 1, '9007199254740990': 2, '9007199254740992': 4, length: 2 ** 53 + 20};
        const kept = Array.prototype.toSpliced.call(like, 0, 2 ** 53 - 3, 'a');
        await control.finish(kept.length === 3 && kept[0] === 'a' && kept[1] === 1 && kept[2] === 2);",
    );
}

/// FIG-5874: admission preserves the generic result, error and observable
/// coercion/park order. Charges intentionally change: the admitted branch
/// pays its executed guard plus the native formula, never the generic loop.
#[test]
fn guarded_search_and_join_match_generic_array_semantics() {
    // SC8's Smith builds dialect-neutral complete programs, rather than JS
    // primitives/coercion hooks. Generate this bounded JS-value matrix here.
    let primitives = [
        "absent", "null", "true", "false", "0.0", "-0.0", "nan", "inf", "-inf", "1.5", "\"a\"",
        "\"😀\"",
    ];
    let starts = [
        "", ", absent", ", nan", ", -0.0", ", -inf", ", inf", ", -2.9", ", 1.9", ", 99.0",
    ];
    for seed in 0..24 {
        let elements = (0..seed % 7)
            .map(|i| {
                if (seed + i) % 5 == 0 {
                    "()"
                } else {
                    primitives[(seed * 7 + i * 3) % primitives.len()]
                }
            })
            .collect::<Vec<_>>()
            .join(", ");
        for method in ["includes", "indexOf", "lastIndexOf"] {
            let needle = primitives[seed % primitives.len()];
            let setup = format!(
                "let xs = [{elements}] let args = [{needle}{}]",
                starts[seed % starts.len()]
            );
            assert_array_paths(method, &setup);
        }
    }
    for method in ["includes", "indexOf", "lastIndexOf"] {
        for start in starts {
            for needle in ["nan", "absent", "0.0", "-0.0"] {
                assert_array_paths(
                    method,
                    &format!("let xs = [(), absent, nan, -0.0, 0.0] let args = [{needle}{start}]"),
                );
            }
        }
        for setup in [
            "let xs = {\"0\": 1.0, length: 1.0} let args = [1.0]",
            "let xs = null let args = []",
            "let xs = [1.0, 2.0] let args = [absent, {valueOf: fn(this, args) { print \"from\" set xs[0] = () do perform 0(2.0) as Any return 0.0 }}]",
            "let xs = [] let args = [null, {valueOf: fn(this, args) { throw \"not reached\" }}]",
            "let object = {x: 1.0} let xs = [object, {x: 1.0}] let args = [object]",
            "let xs = [1.0] let args = [1.0, {valueOf: fn(this, args) { print \"throw\" throw \"bad start\" }}]",
        ] {
            assert_array_paths(method, setup);
        }
    }
    for setup in [
        "let xs = [] let args = []",
        "let xs = [\"a\"] let args = [absent]",
        "let xs = [\"\", \"😀\", \"é\"] let args = [\"|\"]",
        "let xs = [\"a\", \"b\"] let args = []",
        "let xs = [\"a\", (), null, absent, 3.0] let args = [\"|\"]",
        "let xs = [\"a\", \"b\", null, absent, ()] let args = [\"😀\"]",
        "let xs = [\"😀\", \"é\", {toString: fn(this, args) { print \"late element\" return \"z\" }}] let args = [\"|\"]",
        "let xs = [\"a\", \"b\"] let args = [{toString: fn(this, args) { print \"separator\" do perform 0(1.0) as Any set xs[0] = \"changed\" return \"|\" }}]",
        "let xs = [{toString: fn(this, args) { print \"element\" throw \"bad element\" }}] let args = [{toString: fn(this, args) { print \"separator first\" return \"|\" }}]",
        "let xs = {\"0\": \"a\", length: 2.0} let args = []",
        "let xs = null let args = []",
    ] {
        assert_array_paths("join", setup);
    }
}

fn assert_array_paths(method: &str, setup: &str) {
    let fast = observe_array_path(method, setup, false);
    let generic = observe_array_path(method, setup, true);
    assert_eq!(
        (&fast.0, &fast.1),
        (&generic.0, &generic.1),
        "{method}: {setup}"
    );
    // A second execution is not needed for determinism: each implementation
    // is checked against the same generated input and host script.
    assert!(fast.2 > 0 && generic.2 > 0);
}

#[derive(Default)]
struct ArrayPathHost {
    events: Vec<String>,
}

impl Host for ArrayPathHost {
    fn print(&mut self, value: &Datum) {
        self.events.push(format!("print:{value:?}"));
    }
    fn cancel_requested(&mut self) -> bool {
        false
    }
}

fn observe_array_path(method: &str, setup: &str, generic: bool) -> (String, Vec<String>, u64) {
    let mut source = String::from("");
    for (name, _) in kernel().library.iter() {
        source.push_str(&format!("use {name}\n"));
    }
    let prefix = if generic { "generic_" } else { "" };
    source.push_str(&format!(
        "function law.main() -> Any\nkernel 1\nmetering statements\nimport 0 operation host.echo(x: Any) -> Any\nbody {{ {setup} let answer = invoke ts.array.{prefix}{method}(xs, args) return answer }}"
    ));
    let document = super::linked_inspection::main(&source, &kernel().library);
    let mut machine = super::pages::start(
        crate::tests::linked::program(Arc::new(document), kernel().prepared.clone()),
        Bounds {
            memory_meter: lash_kernel_vm::MemoryMeter::On,
            charge: lash_kernel_vm::Charge::Exact,
            charge_limit: 100_000_000,
            memory: 16 << 20,
            call_depth: 200,
            live_tasks: 100,
            requests_per_park: 100,
            join_members: 100,
        },
        CellStart {
            target: Target::Main,
            args: vec![],
            session: None,
            owner: super::pages::OWNER,
            modes: Default::default(),
            host: Vec::new(),
        },
    )
    .unwrap();
    let mut host = ArrayPathHost::default();
    loop {
        match machine.run(&mut host, u64::MAX).unwrap() {
            Step::Ended(end) => {
                let answer = match end {
                    End::Finished(done) => format!("ok:{:?}", done.result),
                    End::Error(error) => format!("error:{error:?}"),
                    other => panic!("{other:?}"),
                };
                return (
                    answer,
                    host.events,
                    machine.meters().charged.expect("exact charge"),
                );
            }
            Step::Parked(park) => {
                host.events.push("park".into());
                for request in park.requests {
                    match request {
                        Request::Effect(effect) => {
                            host.events.push(format!(
                                "effect:{}:{:?}",
                                crate::tests::linked::operation(&effect.use_),
                                effect.args
                            ));
                            machine
                                .deliver(effect.wait, Outcome::Completed(effect.args[0].clone()))
                                .unwrap();
                        }
                    }
                }
            }
            Step::Slice => unreachable!("unbounded slice"),
        }
    }
}

/// ECMA-262 CreateUnmappedArgumentsObject and rest binding own their slots,
/// even when array callbacks share the internal invocation buffer (FIG-5919).
#[test]
fn callback_arguments_and_rest_outlive_their_invocation() {
    law(r#"
        const a = [1, , 3]; const saved = []; const tails = []; const readers = [];
        const result = a.map(function(value, ...rest) {
            saved.push(arguments); tails.push(rest);
            readers.push(() => arguments[0] + rest[0]);
            arguments[0] = value * 10; rest[0] += 10;
            return value;
        });
        const promises = [4, 5].map(async function(value, ...rest) {
            const args = arguments; await sleep(1);
            return args[0] + rest[0] + (args[2] === rest[1] ? 0 : 100);
        });
        const asyncResult = await Promise.all(promises);
        const bound = [6, 7].map(function(prefix, value, index, receiver) {
            return prefix + value + index + (receiver.length === 2 ? 0 : 100);
        }.bind(null, 10));
        const slots = result.length === 3 && !(1 in result) && result[0] === 1 && result[2] === 3;
        const distinct = saved[0] !== saved[1] && saved[0].length === 3 && saved[1].length === 3;
        const values = saved[0][0] === 10 && saved[1][0] === 30;
        const indices = saved[0][1] === 0 && saved[1][1] === 2;
        const receivers = saved[0][2] === a && saved[1][2] === a && tails[0] !== tails[1];
        const captured = readers[0]() === 20 && readers[1]() === 42;
        const asyncAndBound = asyncResult.join(',') === '4,6' && bound.join(',') === '16,18';
        const sync = slots && distinct && values && indices;
        await control.finish(sync && receivers && captured && asyncAndBound);
    "#);
}

/// FIG-7282 / ECMA-402: Node v24.18.0, full ICU 78.3, en-US outputs.
/// Shortest decimal rounding is half-expand, including carry and signed zero.
#[test]
fn locale_formatting_matches_pinned_node_en_us() {
    law(r#"
      function format(value: any) { return value.toLocaleString(); }
      const cases = [
        [1234.5, '1,234.5'], [-0, '-0'],
        [1e21, '1,000,000,000,000,000,000,000'],
        [0.0005, '0.001'], [-0.0005, '-0.001'], [0.0004, '0'],
        [-0.0004, '-0'], [1.2345, '1.235'], [9.9995, '10'],
        [999.9995, '1,000'], [NaN, 'NaN'], [Infinity, '∞'],
        [-Infinity, '-∞'], [Number.MIN_VALUE, '0'],
        [1000000000000000100, '1,000,000,000,000,000,100'],
        [[1234.5, null, undefined, true, 'x', [0.0005, -0]], '1,234.5,,,true,x,0.001,-0'],
        [new Date(0), '1/1/1970, 12:00:00 AM'],
        [new Date('2024-12-31T23:59:58.999Z'), '12/31/2024, 11:59:58 PM'],
        [new Date('0000-01-01T00:00:00Z'), '1/1/1, 12:00:00 AM'],
        [new Date(NaN), 'Invalid Date']
      ];
      for (const pair of cases) {
        const value: any = pair[0];
        const actual = format(value);
        if (actual !== pair[1]) { throw new Error('default: ' + value + ' => ' + actual + ' expected ' + pair[1]); }
        if (value.toLocaleString('en-US') !== pair[1]) { throw new Error('en-US: ' + value); }
        if (value.toLocaleString(undefined) !== pair[1]) { throw new Error('undefined: ' + value); }
      }
      let calls = 0;
      const custom = {toLocaleString(locale) { calls++; return locale; }};
      const customText = [custom, , null, undefined].toLocaleString('en-US');
      if (customText !== 'en-US,,,' || calls !== 1) { throw new Error('custom: ' + customText + ' calls=' + calls); }
      await control.finish(true);
    "#);
}

/// FIG-7282: unsupported locale forms name their method both at admission
/// and when an unknown receiver reaches the runtime locale boundary.
#[test]
fn locale_refusals_name_the_method_for_every_receiver_form() {
    for source in [
        "(3).toLocaleString('de-DE');",
        "const x: number = 3; x.toLocaleString(undefined, {style:'currency'});",
        "[1].toLocaleString('de-DE');",
        "'x'.toLocaleString('de-DE');",
        "Intl.NumberFormat();",
        "const formatter = Intl.NumberFormat;",
        "new Intl.NumberFormat();",
    ] {
        let error = super::lower(source).expect_err(source);
        assert_eq!(
            error.code,
            crate::DiagnosticCode::MethodUnsupported,
            "{source}"
        );
        assert!(error.is_dialect_refusal(), "{error}");
        assert!(error.to_string().contains("toLocaleString"), "{error}");
        assert!(error.to_string().contains("en-US"), "{error}");
    }
    for receiver in ["3", "[3]", "new Date(0)", "'x'"] {
        for arguments in ["'de-DE'", "undefined, {style:'currency'}"] {
            let source = format!(
                "function render(x: any) {{ return x.toLocaleString({arguments}); }} const l = {{value: {receiver}}}; render(l.value);"
            );
            let end = run(&source);
            assert!(
                matches!(end, End::Error(lash_kernel_vm::RunError::Uncaught { error: Datum::Error(ref error), .. })
                if error.kind == "TS_METHOD_UNSUPPORTED" && error.message.contains("toLocaleString") && error.message.contains("en-US")),
                "{end:?}\n{source}"
            );
        }
    }
}

/// FIG-7308 / ECMA-262 EvaluateCall: missing and non-callable methods throw
/// a named guest TypeError after arguments, including computed/optional members.
/// FIG-7282: known unsupported methods on typed receivers remain refusals.
#[test]
fn missing_methods_name_the_receiver_and_preserve_argument_effects() {
    for (receiver, kind) in [
        ("3", "Number"),
        ("'x'", "Text"),
        ("[]", "List"),
        ("true", "Bool"),
        ("new Map()", "Map"),
        ("new Set()", "Set"),
    ] {
        let source = format!("const value = {receiver}; value.missingMethod();");
        let error = super::lower(&source).expect_err(&source);
        assert_eq!(error.code, crate::DiagnosticCode::MethodUnsupported);
        let text = error.to_string();
        assert!(
            text.contains("missingMethod") && text.contains(kind),
            "{text}"
        );
    }
    for receiver in ["{}", "{missingMethod: 0}", "{missingMethod: null}"] {
        for call in [
            "value.missingMethod(effect())",
            "value['missingMethod'](effect())",
            "value[key](effect())",
            "value?.missingMethod(effect())",
            "(value?.missingMethod)(effect())",
            "make().missingMethod(effect())",
        ] {
            law(&format!(
                r#"
          let effects = 0;
          function effect() {{ effects++; return 0; }}
          function make(): any {{ return {receiver}; }}
          const value: any = make(); const key = 'missingMethod';
          let refused = false;
          try {{ {call}; }} catch (e) {{
            refused = e instanceof TypeError && e.name === 'TypeError' &&
              e.message.includes('missingMethod') && e.message.includes('record');
          }}
          await control.finish(refused && effects === 1);
        "#
            ));
        }
    }
    law(r#"let reads = 0; let effects = 0;
        const value: any = {}; const key = {toString() { reads++; return 'missingMethod'; }};
        let refused = false;
        try { value[key](effects++); } catch (e) { refused = e instanceof TypeError && e.message.includes('missingMethod'); }
        await control.finish(refused && reads === 1 && effects === 1);"#);
    law(r#"
        let order = '';
        const value: any = {missingMethod: 0};
        function receiver(): any { order += 'receiver,'; return value; }
        function key() { order += 'key,'; return 'missingMethod'; }
        function argument() { order += 'argument'; value.missingMethod = () => true; }
        let caught = false;
        try { receiver()[key()](argument()); } catch (e) {
          caught = e instanceof TypeError && e.message.includes('missingMethod') && e.message.includes('record');
        }
        await control.finish(caught && order === 'receiver,key,argument');
    "#);
    law("const value: any = null; await control.finish(value?.missingMethod() === undefined);");
    law("const value: any = {}; await control.finish(value.missingMethod?.() === undefined);");
}

/// K-CHG-007 / FIG-7282: binary64 conversion and the bounded rounding
/// prefix cost at most 2000; grouping/layout scan at most 48 forms per
/// output byte, including the 309-digit largest finite number.
#[test]
fn locale_number_charge_bounds_conversion_rounding_and_grouping() {
    let library = super::library();
    let registry = super::machine::registry();
    for name in ["ts.number.toLocaleString", "ts.locale_element"] {
        let id = lash_kernel_dialect::Library::resolve(library, name).unwrap();
        let formula = registry
            .get(&id)
            .unwrap()
            .definition()
            .charge()
            .unwrap()
            .clone();
        for bytes in [1_u64, 6, 29, 411] {
            let charge = formula.evaluate(&mut |operand, _| match operand {
                lash_kernel_doc::Operand::Result => bytes + 1,
                lash_kernel_doc::Operand::Param(_) => 1,
            });
            assert!(
                charge >= 2000 + 48 * bytes,
                "{bytes} output bytes need bounded digit work, got {charge}"
            );
        }
    }
}
