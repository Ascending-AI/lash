//! The laws of where a run publishes its execution roots to its heap
//! (FIG-6318): only where a collection can observe them, so a collection
//! anywhere never frees what the run holds, and a slice of plain
//! statements publishes a constant number of times however long it runs.

use lash_kernel_doc::Datum;

use super::embedder::{Embedder, Setup};
use crate::machine::Boundary;
use crate::{Bounds, End, Layout, Machine, Step};

/// The values a generated program keeps in variables across statements,
/// by kind, so that every statement it is made of type-checks. A value
/// names only values, a list values and lists, a record values and lists,
/// and only a list is changed in place, by appending a value: what the run
/// returns holds no cycle.
const LISTS: usize = 3;
const RECORDS: usize = 2;
const VALUES: usize = 4;
const CLOSURES: usize = 2;
const RESULTS: usize = 2;

struct Bits(u64);

impl Bits {
    fn below(&mut self, n: usize) -> usize {
        // SplitMix64: a fixed seed spells one program.
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        ((z ^ (z >> 31)) % n as u64) as usize
    }
}

/// A program from `seed`: a loop whose body builds lists, records and
/// closures over the variables, calls them, drops values to garbage,
/// waits on effects and joins spawned tasks, then returns everything it
/// still holds, so a value freed while live changes the result or faults.
fn generated(seed: u64) -> String {
    let mut bits = Bits(seed);
    let mut body = String::new();
    for n in 0..8 + bits.below(10) {
        let (l, r, v, f) = (
            bits.below(LISTS),
            bits.below(RECORDS),
            bits.below(VALUES),
            bits.below(CLOSURES),
        );
        let (l2, v2) = (bits.below(LISTS), bits.below(VALUES));
        body.push_str(&match bits.below(11) {
            0 => format!("set l{l} = [v{v}, v{v2}]\n"),
            1 => format!("set l{l}[list.len(l{l})] = v{v}\n"),
            2 => format!("set r{r} = {{a: v{v}, b: l{l}}}\n"),
            3 => format!("set v{v} = r{r}.a\n"),
            4 => format!("set f{f} = fn() {{ return [v{v}, l{l}, i] }}\n"),
            5 => format!("let a{n} = apply f{f}()\nset g{} = a{n}\n", bits.below(RESULTS)),
            6 => format!("set v{v} = text.concat(\"t\", \"u\")\n"),
            7 => format!("set l{l} = []\nset v{v2} = [v{v}]\n"),
            8 => format!("let e{n} = perform echo(v{v}) as Any\nset v{v2} = [e{n}, v{v}]\n"),
            9 => format!(
                "let h{n} = spawn call worker(v{v})\nlet j{n} = join h{n}\nset l{l} = [j{n}, l{l2}]\n"
            ),
            _ => format!("set v{v} = work.fill(3)\n"),
        });
    }
    let iterations = 3 + bits.below(4);
    let declare = |prefix: &str, count: usize, initial: &str| -> String {
        (0..count)
            .map(|k| format!("let {prefix}{k} = {initial}\n"))
            .collect()
    };
    let lists = declare("l", LISTS, "[]");
    let records = declare("r", RECORDS, "{a: 0, b: []}");
    let values = declare("v", VALUES, "\"v\"");
    let closures = declare("f", CLOSURES, "fn() { return \"f\" }");
    let results = declare("g", RESULTS, "\"g\"");
    format!(
        "fn worker(x) {{ let w = perform echo(x) as Any return [w, x] }}\n\
         main {{\n{lists}{records}{values}{closures}{results}\
         let i = 0\n\
         while num.lt(i, {iterations}) {{\n{body}set i = num.add(i, 1)\n}}\n\
         let c0 = apply f0()\nlet c1 = apply f1()\n\
         return [l0, l1, l2, r0, r1, v0, v1, v2, v3, g0, g1, c0, c1]\n}}"
    )
}

/// Runs `text` to its end in slices of `slice` units, the hook `boundary`
/// deciding at each statement boundary, answering effects oldest first.
fn ended(text: &str, slice: u64, boundary: impl FnMut(u64) -> Boundary + 'static) -> End {
    let mut embedder = Embedder::with(
        text,
        Setup {
            slice,
            ..Setup::default()
        },
    );
    embedder.machine.boundary = Some(Box::new(boundary));
    embedder.run_to_end(&[])
}

fn result(end: End) -> Datum {
    match end {
        End::Finished(finished) => finished.result,
        other => panic!("the run did not finish: {other:?}"),
    }
}

/// A collection forced at any statement boundary, mid-slice, through the
/// machine's collection point frees nothing the run holds: every generated
/// program ends as it does with none. One asked of the heap directly, with
/// nothing published, is refused or traces current roots, and frees
/// nothing either.
#[test]
fn a_collection_at_any_statement_boundary_never_frees_a_live_value() {
    for seed in 0..8 {
        let text = generated(seed);
        let reference = result(ended(&text, u64::MAX, |_| Boundary::Go));
        for slice in [u64::MAX, 40] {
            for (name, hook) in [
                (
                    "every",
                    Box::new(|_| Boundary::Collect) as Box<dyn FnMut(u64) -> Boundary>,
                ),
                ("unpublished", Box::new(|_| Boundary::Unpublished)),
                (
                    "some",
                    Box::new(move |at| {
                        if (at ^ seed) % 3 == 0 {
                            Boundary::Collect
                        } else {
                            Boundary::Go
                        }
                    }),
                ),
            ] {
                assert_eq!(
                    result(ended(&text, slice, hook)),
                    reference,
                    "seed {seed} slice {slice} {name}\n{text}"
                );
            }
        }
    }
}

/// A node that dies mid-slice, after collections at every boundary of the
/// slice, leaves its last checkpoint whole: the run opened from it on
/// another node ends as one never interrupted.
#[test]
fn a_cell_killed_mid_slice_after_collections_resumes_from_its_checkpoint() {
    for seed in 0..6 {
        let text = generated(seed);
        let reference = result(ended(&text, u64::MAX, |_| Boundary::Go));
        let mut embedder = Embedder::with(
            &text,
            Setup {
                slice: 40,
                ..Setup::default()
            },
        );
        let mut checkpoint = (embedder.checkpoint(), embedder.pending.clone());
        let kill_at = 3 + seed * 2;
        embedder.machine.boundary = Some(Box::new(move |at| {
            if at == kill_at {
                Boundary::Kill
            } else {
                Boundary::Collect
            }
        }));
        let mut killed = false;
        let end = loop {
            match embedder.machine.run(&mut embedder.world, 40) {
                Ok(Step::Ended(end)) => break end,
                Ok(Step::Slice) => {}
                Ok(Step::Parked(park)) => {
                    embedder.requests.extend(park.requests.iter().cloned());
                    embedder.pending.extend(park.requests.iter().cloned());
                    embedder.deliver_next();
                }
                Err(error) => {
                    assert!(
                        !killed && error.to_string().contains("the node died mid-slice"),
                        "seed {seed}: the planned death alone ends a slice: {error}"
                    );
                    killed = true;
                    let ((root, certificate), pending) = checkpoint.clone();
                    embedder.open_at(&root, certificate, Layout::default(), true);
                    embedder.pending = pending;
                    continue;
                }
            }
            checkpoint = (embedder.checkpoint(), embedder.pending.clone());
        };
        assert!(
            killed,
            "seed {seed}: the run ended before boundary {kill_at}"
        );
        assert_eq!(result(end), reference, "seed {seed}\n{text}");
    }
}

/// A slice of plain statements publishes the run's roots a constant number
/// of times, however many statements it runs: none of them is a point a
/// collection observes.
#[test]
fn a_slice_publishes_its_roots_a_constant_number_of_times() {
    let publishes = |n: usize| {
        let items = (0..n).map(|k| k.to_string()).collect::<Vec<_>>().join(", ");
        let text = format!(
            "main {{\n\
             let out = []\n\
             let f = fn(x) {{ return {{a: x, b: [x, out]}} }}\n\
             for x in [{items}] {{\n\
               let r = apply f(x)\n\
               set out[x] = [r, num.add(x, 1)]\n\
             }}\n\
             return list.len(out)\n}}"
        );
        let mut embedder = Embedder::with(
            &text,
            Setup {
                bounds: Bounds {
                    charge: u64::MAX,
                    ..super::embedder::ROOMY
                },
                ..Setup::default()
            },
        );
        crate::perf_witness::enable_work();
        let end = embedder.machine.run(&mut embedder.world, u64::MAX).unwrap();
        let work = crate::perf_witness::take_work().unwrap();
        assert!(
            matches!(&end, Step::Ended(End::Finished(finished)) if finished.result == super::embedder::int(n as i64)),
            "{end:?}"
        );
        (work.root_publishes, work.vm_steps)
    };
    let (few, few_steps) = publishes(10);
    let (many, many_steps) = publishes(1_000);
    assert!(many_steps > 10 * few_steps, "{few_steps} {many_steps}");
    assert_eq!(
        many, few,
        "publishes over {few_steps} and {many_steps} statements"
    );
    assert!(few <= 3, "{few} publishes in one slice");
}
