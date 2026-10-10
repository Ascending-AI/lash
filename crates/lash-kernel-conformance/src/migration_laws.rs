//! The laws of the migration from kernel version 1 to its synthetic
//! successor (`K-VER-004`, `K-VER-005`).

use std::sync::Arc;

use lash_kernel_doc::{
    Datum, ErrorDatum, FunctionId, FunctionRegistry, Integer, KernelVersion, Type, Unit,
    parse_definition, parse_document,
};
use lash_kernel_migrate::{ParkedRefusal, migrate_registry, migration_from};
use lash_kernel_vm::{Bindings, KernelMachine, Machine, Program, Start, Step, Target};

use crate::case::{CaseBounds, Delivery, ScriptOutcome};
use crate::{Case, Environment, Expected, ExpectedEnd, Trace, check_migration};

/// A library for both versions: the machine's own functions and one with a
/// kernel body that calls its argument, each redeclared for the successor.
fn library() -> (Arc<FunctionRegistry>, FunctionId) {
    let mut registry = FunctionRegistry::new();
    lash_kernel_vm::register_machine_functions(&mut registry).expect("machine functions");
    let apply = parse_definition(
        "function each.apply(f: Fn(x: Any) -> Any, x: Any) -> Any\nkernel 1\ncharge 1\n\
         body { let r = apply f(x) return r }\n",
    )
    .expect("a library function with a kernel body");
    let apply = registry.register(apply, None).expect("register it");
    let migration = migration_from(KernelVersion::One).expect("version 1 has a successor");
    migrate_registry(&mut registry, migration).expect("redeclare the library");
    (Arc::new(registry), apply)
}

fn int(value: i64) -> Datum {
    Datum::Int(Integer::from(value))
}

fn text(value: &str) -> Datum {
    Datum::Text(value.to_owned())
}

/// Two tasks, each in a loop of effects under a cleanup block that performs
/// one more; one effect fails and is caught. The run parks five times: with
/// both tasks in their loops, with one mid-loop and the other in its
/// handler, and with each in its cleanup block.
fn fan_out(apply: FunctionId) -> Case {
    let document = format!(
        r#"kernel 1
numbers by_spelling
effect step(n: Int) -> Int
effect close(name: Text) -> Any
use each.apply = @{apply}
fn same(x) {{ return x }}
fn worker(name, items) {{
  let last = 0
  try {{
    for item in items {{
      let got = perform step(item) as Int
      print got
      set last = got
    }}
  }} catch error {{
    print "caught"
  }} finally {{
    do perform close(name) as Any
    print name
  }}
  return last
}}
main {{
  let ones = [1, 2]
  let others = [3]
  let a = spawn call worker("a", ones)
  let b = spawn call worker("b", others)
  let tasks = [a, b]
  let answers = join all tasks
  let checked = invoke each.apply(&same, answers)
  finish checked
}}
"#
    );
    let completed = |request, value| {
        vec![Delivery {
            request,
            outcome: ScriptOutcome::Completed(value),
            dropped: false,
        }]
    };
    let step = |n| Trace::Effect {
        identity: None,
        effect: "step".to_owned(),
        args: vec![int(n)],
        result: Type::Int,
    };
    let close = |name| Trace::Effect {
        identity: None,
        effect: "close".to_owned(),
        args: vec![text(name)],
        result: Type::Any,
    };
    Case {
        name: "two tasks, each in a loop under a cleanup block".to_owned(),
        document,
        environment: Environment {
            bounds: CaseBounds::default(),
            deliveries: vec![
                completed(0, int(10)),
                vec![Delivery {
                    request: 1,
                    outcome: ScriptOutcome::Failed(ErrorDatum {
                        kind: "unavailable".to_owned(),
                        message: "the host is away".to_owned(),
                        data: Datum::Null,
                    }),
                    dropped: false,
                }],
                completed(2, int(20)),
                completed(3, Datum::Null),
                completed(4, Datum::Null),
            ],
            ..Environment::default()
        },
        expected: Expected {
            prints: vec![int(10), text("caught"), int(20), text("b"), text("a")],
            end: ExpectedEnd::Finished(Datum::List(vec![int(20), int(0)])),
            trace: vec![step(1), step(3), step(2), close("b"), close("a")],
            charged: None,
            parks: Some(5),
        },
    }
}

/// `K-VER-005`: parked under version 1 at each park and resumed under the
/// successor, the run prints, ends and identifies every effect as the run
/// that stayed under version 1 does.
#[test]
fn a_run_parked_at_any_park_resumes_under_the_next_kernel_version_as_it_would_have_run() {
    let (registry, apply) = library();
    let migration = migration_from(KernelVersion::One).expect("version 1 has a successor");
    let case = fan_out(apply);
    let check = check_migration::<KernelMachine>(migration, &registry, &case)
        .unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(check.migrated.len(), 5, "one migrated run per park");

    // The runs that resumed really ran under the successor: its document
    // is stored with `print` spelled `emit`, a version 1 reader refuses
    // it, and a loop test costs it 2.
    let written = parse_document(&case.document).expect("the case's document");
    let rewritten = (migration.document)(&written, registry.as_ref()).expect("rewritten");
    assert_eq!(
        rewritten.document.manifest.kernel,
        KernelVersion::SyntheticNext.number()
    );
    let stored = rewritten.document.to_json().expect("stored form");
    assert!(stored.contains(r#"{"emit":"#) && !stored.contains(r#"{"print":"#));
    assert_eq!(
        lash_kernel_doc::Document::from_json(&stored).expect("read back"),
        rewritten.document
    );
    assert_ne!(
        rewritten.functions.get(&apply),
        Some(&apply),
        "a redeclared function is a new identity"
    );
    assert!(
        check.migrated[0].charged > check.stayed.charged,
        "migrated at the first park, every later loop test is priced by the successor"
    );
}

/// `K-VER-004`: a run that stands in a library function's body is one the
/// synthetic migration declares it cannot carry, and it says so with the
/// site.
#[test]
fn a_run_parked_in_a_library_body_is_refused_with_the_site_it_stands_on() {
    let (registry, apply) = library();
    let migration = migration_from(KernelVersion::One).expect("version 1 has a successor");
    let document = parse_document(&format!(
        "kernel 1\nnumbers by_spelling\neffect step(n: Int) -> Int\nuse each.apply = @{apply}\n\
         main {{ let ask = fn(x) {{ let r = perform step(x) as Int return r }} \
         let y = invoke each.apply(ask, 1) finish y }}\n"
    ))
    .expect("a document that waits inside a library body");
    let program = Program {
        document: Arc::new(document),
        library: lash_kernel_vm::PreparedLibrary::new(Arc::clone(&registry)),
    };
    let start = Start {
        target: Target::Main,
        args: Vec::new(),
        bindings: Bindings::default(),
    };
    let mut machine =
        KernelMachine::start(program.clone(), CaseBounds::default().into(), start).expect("start");
    struct NoHost;
    impl lash_kernel_vm::Host for NoHost {
        fn clock(&mut self) -> lash_kernel_doc::Timestamp {
            lash_kernel_doc::Timestamp {
                nanoseconds: Integer::from(0),
            }
        }
        fn random(&mut self) -> u64 {
            0
        }
        fn read(
            &mut self,
            _handle: &lash_kernel_doc::Handle,
            _request: &Datum,
        ) -> Result<Datum, ErrorDatum> {
            Ok(Datum::Null)
        }
        fn print(&mut self, _value: &Datum) {}
        fn cancel_requested(&mut self) -> bool {
            false
        }
    }
    let step = machine.run(&mut NoHost, u64::MAX).expect("run to the park");
    assert!(matches!(step, Step::Parked(_)), "{step:?}");
    let parked = machine.export().expect("export");

    let rewritten =
        (migration.document)(&program.document, registry.as_ref()).expect("the document carries");
    let refusal = (migration.parked)(&parked, &program.document, &rewritten)
        .expect_err("the run stands in a library body");
    assert!(
        matches!(
            &refusal,
            ParkedRefusal::SiteNotCarried { site, .. } if site.unit == Unit::Library(apply)
        ),
        "{refusal:?}"
    );
}

/// What a run of `document` printed, what it finished with and what it was
/// charged.
fn ran(
    document: lash_kernel_doc::Document,
    registry: &Arc<FunctionRegistry>,
) -> (Vec<Datum>, Datum, u64) {
    struct Prints(Vec<Datum>);
    impl lash_kernel_vm::Host for Prints {
        fn clock(&mut self) -> lash_kernel_doc::Timestamp {
            lash_kernel_doc::Timestamp {
                nanoseconds: Integer::from(0),
            }
        }
        fn random(&mut self) -> u64 {
            0
        }
        fn read(
            &mut self,
            _handle: &lash_kernel_doc::Handle,
            _request: &Datum,
        ) -> Result<Datum, ErrorDatum> {
            Ok(Datum::Null)
        }
        fn print(&mut self, value: &Datum) {
            self.0.push(value.clone());
        }
        fn cancel_requested(&mut self) -> bool {
            false
        }
    }
    lash_kernel_doc::validate_document(&document, registry.as_ref())
        .expect("the document is one its version's library runs");
    let program = Program {
        document: Arc::new(document),
        library: lash_kernel_vm::PreparedLibrary::new(Arc::clone(registry)),
    };
    let start = Start {
        target: Target::Main,
        args: Vec::new(),
        bindings: Bindings::default(),
    };
    let mut machine =
        KernelMachine::start(program, CaseBounds::default().into(), start).expect("start");
    let mut host = Prints(Vec::new());
    let step = machine.run(&mut host, u64::MAX).expect("run to the end");
    let Step::Ended(lash_kernel_vm::End::Finished(finished)) = step else {
        panic!("the run did not finish: {step:?}");
    };
    (host.0, finished.result, machine.meters().charged)
}

/// `K-VER-004`, for a function a session saved: stored under version 1,
/// rewritten by the document migration alone, and called under the
/// successor by a document of the successor it is installed in. It prints
/// and answers what it does under version 1, with its capture as it was
/// frozen, and is charged the successor's price for its loop.
#[test]
fn a_function_saved_under_one_version_is_migrated_and_called_under_the_next() {
    use lash_kernel_dialect::{SavedFunction, install};
    use lash_kernel_doc::Name;

    let (registry, _) = library();
    let migration = migration_from(KernelVersion::One).expect("version 1 has a successor");
    let saved = SavedFunction {
        name: Name::new("tail"),
        document: parse_document(
            "kernel 1\nnumbers by_spelling\n\
             fn tail(items) { let last = fallback for item in items { print item set last = item } \
             return last }\n\
             main { finish null }\n",
        )
        .expect("the saved function's document"),
        captures: [(Name::new("fallback"), text("none"))].into(),
        written: None,
        token: None,
    };
    let caller = parse_document(
        "kernel 1\nnumbers by_spelling\n\
         main { let items = [\"a\", \"b\"] let last = call tail(items) finish last }\n",
    )
    .expect("the calling document");
    let installed = |caller: &lash_kernel_doc::Document, function: &SavedFunction| {
        let mut document = caller.clone();
        install(
            &mut document,
            &[(function.name.clone(), function.clone())].into(),
            &[function.name.clone()].into(),
            &lash_kernel_dialect::FunctionValues::Bare,
            &Default::default(),
            &Default::default(),
            &|library| registry.get(library).is_some(),
        )
        .expect("the function installs");
        document
    };

    let (prints, answer, charged) = ran(installed(&caller, &saved), &registry);
    assert_eq!(prints, [text("a"), text("b")]);
    assert_eq!(answer, text("b"));

    let migrated = lash_kernel_migrate::saved_function(migration, &saved, registry.as_ref())
        .expect("a saved function migrates alone");
    assert_eq!(
        migrated.document.manifest.kernel,
        KernelVersion::SyntheticNext.number()
    );
    assert_eq!(migrated.captures, saved.captures);
    let stored = migrated.document.to_json().expect("stored form");
    assert!(
        stored.contains("\"emit\"") && !stored.contains("\"print\""),
        "the migrated function is stored in the successor's spelling: {stored}"
    );
    let next_caller = (migration.document)(&caller, registry.as_ref())
        .expect("the calling document carries")
        .document;
    let (next_prints, next_answer, next_charged) =
        ran(installed(&next_caller, &migrated), &registry);
    assert_eq!(next_prints, prints);
    assert_eq!(next_answer, answer);
    // The loop tests its continuation three times, each one dearer.
    assert_eq!(next_charged, charged + 3);
}
