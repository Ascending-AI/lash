//! JavaScript array laws: sparse slots, length snapshots, live iteration and
//! callbacks run on the real kernel library, never a native stand-in.

use std::collections::BTreeSet;
use std::sync::{Arc, OnceLock};

use lash_kernel_dialect::{Environment, NamedLibrary};
use lash_kernel_doc::{Datum, ErrorDatum, FunctionRegistry, Handle, Integer, Timestamp};
use lash_kernel_vm::{
    Bindings, Bounds, End, Host, KernelMachine, Machine, Outcome, Program, Request, Start, Step,
    Target,
};

struct Kernel {
    library: NamedLibrary,
    registry: Arc<FunctionRegistry>,
}

fn kernel() -> &'static Kernel {
    static KERNEL: OnceLock<Kernel> = OnceLock::new();
    KERNEL.get_or_init(|| {
        let mut registry = super::kernel_registry();
        let mut library = NamedLibrary::from_registry(&registry).unwrap();
        for definition in crate::define_helpers(&mut library).unwrap() {
            registry.register(definition, None).unwrap();
        }
        Kernel {
            library,
            registry: Arc::new(registry),
        }
    })
}

#[derive(Default)]
struct NoHost;

impl Host for NoHost {
    fn clock(&mut self) -> Timestamp {
        Timestamp {
            nanoseconds: Integer::from(0),
        }
    }
    fn random(&mut self) -> u64 {
        0
    }
    fn read(&mut self, _handle: &Handle, _request: &Datum) -> Result<Datum, ErrorDatum> {
        Err(ErrorDatum {
            kind: "no_projection".into(),
            message: "array laws read no projection".into(),
            data: Datum::Null,
        })
    }
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
    let environment = Environment {
        library: &kernel.library,
        effects: &super::machine::effects(),
        controls: super::controls(),
        bindings: &BTreeSet::new(),
        functions: &std::collections::BTreeMap::new(),
    };
    let lowered =
        crate::lower(source, &environment).unwrap_or_else(|error| panic!("{error}\n{source}"));
    drive_document(lowered.document, expected, source)
}

fn drive_document(
    document: lash_kernel_doc::Document,
    expected: &[Vec<Datum>],
    source: &str,
) -> End {
    let kernel = kernel();
    let text = lash_kernel_doc::print_document(&document);
    let program = Program {
        document: Arc::new(document),
        registry: Arc::clone(&kernel.registry),
    };
    let bounds = Bounds {
        charge: 100_000_000,
        memory: 16 << 20,
        call_depth: 200,
        live_tasks: 100,
        requests_per_park: 100,
        join_members: 100,
    };
    let mut machine = KernelMachine::start(
        program,
        bounds,
        Start {
            target: Target::Main,
            args: Vec::new(),
            bindings: Bindings::default(),
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
                        Request::Sleep(request) => {
                            machine.deliver(request.wait, Outcome::Elapsed).unwrap();
                        }
                        Request::Effect(effect) if effect.effect.as_str() == "finish" => {
                            finished_with =
                                Some(effect.args.first().cloned().unwrap_or(Datum::Null));
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
        await finish(before && alias === a && alias.length === 3 && !(4 in a));
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
        await finish(indices === '012' && mapped.join(',') === '11,12,13'
            && sparse.length === 3 && !(1 in sparse) && sparse[2] === 6);
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
        await finish(started && result.join(',') === '2,4,6' && order === '123123');
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
        await finish(ordered && alias === a && records.map(x => x.id).join('') === 'cab'
            && equal.join(',') === '2,1');
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
        await finish(sparseRemoved && a.join(',') === 'x,1,1'
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
        await finish(visited === '1u34' && spread.length === 5 && spread[4] === 5
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
        await finish(sparse && holes && nans && bounds && reduced === 8 && reverse === '431');
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
        await finish(first && flat.join(',') === '1,2,3' && flatMapped.join(',') === '1,2,2,4');
    "#);
}

/// Invalid lengths raise RangeError, and a typed read of a present index
/// containing a hole gives undefined rather than the internal marker.
#[test]
fn invalid_lengths_raise_and_typed_sparse_reads_hide_holes() {
    for length in ["-1", "1.5", "Infinity", "4294967296"] {
        let source = format!("const a = [1, , 3]; a.length = {length}; await finish(true);");
        assert!(
            matches!(run(&source), End::Error(lash_kernel_vm::RunError::Uncaught(Datum::Error(error))) if error.kind == "RangeError"),
            "{source}"
        );
    }
    law("const a: number[] = [1, , 3]; await finish(a[1] === undefined && a.length === 3);");
}

/// The final result exposes JSON-shaped array slots, including nested aliases,
/// while a boundary copy leaves the guest's sparse property presence intact.
#[test]
fn final_results_normalize_holes_without_mutating_guest_arrays() {
    let end = run("const a = [1, , undefined]; const copied = [a, a]; await finish(copied);");
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
        const a = [, 2]; const p = echo(a); await p;
        await echo({nested: a});
        await finish(!(0 in a) && a.length === 2 && a[1] === 2);
    "#,
        &expected,
    );
    assert!(matches!(end, End::Finished(finished) if finished.result == Datum::Bool(true)));
}

/// The dialect's boundary adapter gives native JSON nulls for holes and
/// undefined slots, with no mutation of the guest array used to construct it.
#[test]
fn json_encoding_of_boundary_arrays_uses_null_for_sparse_slots() {
    let kernel = kernel();
    let uses = kernel
        .library
        .iter()
        .map(|(name, id)| format!("use {name} = @{id}\n"))
        .collect::<String>();
    let text = format!(
        "kernel 1\nnumbers float\n{uses}main {{\nlet guest = [(), absent]\nlet copied = invoke ts.boundary(guest)\nlet encoded = json.stringify(copied)\nif same(guest[0], ()) {{}} else {{ throw \"guest was changed\" }}\nfinish encoded\n}}\n"
    );
    let document = lash_kernel_doc::parse_document(&text).unwrap();
    let end = drive_document(document, &[], &text);
    assert!(
        matches!(end, End::Finished(finished) if finished.result == Datum::Text("[null,null]".into()))
    );
}

/// A RegExp result is an array with extra own properties: Array callbacks
/// receive the original result, and today's match-write refusal stays typed.
#[test]
fn regexp_match_arrays_keep_the_callback_receiver_and_write_refusal() {
    law(r#"
        const match = /a(.)/.exec('ab');
        const original = match.map((value, index, receiver) => receiver === match);
        await finish(Array.isArray(match) && original.every(value => value)
            && match.length === 2 && match[1] === 'b');
    "#);
    let end = run("const match = /a(.)/.exec('ab'); match.push('c'); await finish(true);");
    assert!(
        matches!(end, End::Error(lash_kernel_vm::RunError::Uncaught(Datum::Error(error))) if error.kind == "TS_ARRAY_LIKE_MATCH_UNSUPPORTED")
    );
}

/// Named properties are still outside the dialect, including keys produced
/// by negative/fractional indices and the non-index uint32 upper bound.
#[test]
fn non_index_array_writes_keep_the_existing_typed_refusal() {
    for key in ["-1", "1.5", "4294967295", "'extra'"] {
        let source = format!("const a = [1]; a[{key}] = 9; await finish(true);");
        assert!(
            matches!(run(&source), End::Error(lash_kernel_vm::RunError::Uncaught(Datum::Error(error))) if error.kind == "TS_ARRAY_NON_INDEX_PROPERTY_UNSUPPORTED"),
            "{source}"
        );
    }
}

/// TS_TYPED_NUMBER_ADD checks the value read after an untyped alias writes
/// through the same array; it does not coerce a text into a number.
#[test]
fn typed_array_alias_writes_are_checked_at_the_typed_use() {
    let source = "const xs: number[] = [1, 2]; const alias: any = xs; alias[0] = 'a'; await finish(xs[0] + 1);";
    assert!(
        matches!(run(source), End::Error(lash_kernel_vm::RunError::Uncaught(Datum::Error(error))) if error.kind == "type_error")
    );
    law(
        "const xs: any = [1, 2]; const alias: any = xs; alias[0] = 'a'; await finish(xs[0] + 1 === 'a1');",
    );
}

/// Declared aliases, interfaces, function results and for-of elements retain
/// their types while the iterator takes live steps over an array.
#[test]
fn live_for_of_carries_declared_element_and_return_types() {
    let source = "type Price = number; interface Item { price: Price } function total(items: Item[]): number { let sum = 0; for (const item of items) { sum += item.price; } return sum; }";
    law(&format!(
        "{source} await finish(total([{{price: 2}}, {{price: 3}}]) * 2 === 10);"
    ));
    assert!(
        matches!(run(&format!("{source} await finish(total([{{price: '2' as any}}]));")), End::Error(lash_kernel_vm::RunError::Uncaught(Datum::Error(error))) if error.kind == "type_error")
    );
}

/// Array string conversion invokes a generic receiver's own join, and locale
/// conversion retains the dialect's existing deterministic-format refusal.
#[test]
fn string_conversion_obeys_own_join_and_keeps_locale_refusal() {
    law(
        "const a = {0: 1, length: 1, join: function() { return this[0] + 10; }}; await finish(Array.prototype.toString.call(a) === 11 && Array.prototype.toString.call({length: 1}) === '[object Object]');",
    );
    let lowered =
        super::lower("[1].toLocaleString();").expect_err("locale formatting remains refused");
    assert_eq!(lowered.code, crate::DiagnosticCode::MethodUnsupported);
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
            "const huge: any = {{}}; huge[0] = 'x'; huge[4294967295] = 'y'; huge.length = 4294967296; {call}; await finish(true);"
        );
        assert!(
            matches!(run(&source), End::Error(lash_kernel_vm::RunError::Uncaught(Datum::Error(error))) if error.kind == "RangeError"),
            "{source}"
        );
    }
    law(
        "const like = {'9007199254740989': 1, '9007199254740990': 2, '9007199254740992': 4, length: 2 ** 53 + 20};
        const kept = Array.prototype.toSpliced.call(like, 0, 2 ** 53 - 3, 'a');
        await finish(kept.length === 3 && kept[0] === 'a' && kept[1] === 1 && kept[2] === 2);",
    );
}
