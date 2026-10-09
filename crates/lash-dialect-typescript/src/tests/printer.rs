//! K-DIALECT-001: printed kernel source preserves behavior, including edits.
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use lash_kernel_dialect::{Environment, NamedLibrary, Printer};
use lash_kernel_doc as k;
use lash_kernel_edit::{Draft, Edit, Position, Transaction};
use lash_kernel_vm::{
    Bounds, End, Host, KernelMachine, Machine, Outcome, Program, Request, Start, Step, Target,
};

use crate::TypeScript;

#[derive(Default)]
struct Observer {
    trace: Vec<String>,
}
impl Host for Observer {
    fn clock(&mut self) -> k::Timestamp {
        self.trace.push("clock".into());
        k::Timestamp {
            nanoseconds: k::Integer::from(123),
        }
    }
    fn random(&mut self) -> u64 {
        self.trace.push("random".into());
        321
    }
    fn read(&mut self, handle: &k::Handle, request: &k::Datum) -> Result<k::Datum, k::ErrorDatum> {
        self.trace.push(format!("read {handle:?} {request:?}"));
        Ok(k::Datum::Text("answer".into()))
    }
    fn print(&mut self, value: &k::Datum) {
        self.trace.push(format!("print {value:?}"));
    }
    fn cancel_requested(&mut self) -> bool {
        false
    }
}
fn run(
    document: &k::Document,
    registry: &Arc<k::FunctionRegistry>,
    target: Target,
) -> (End, Vec<String>) {
    let mut machine = KernelMachine::start(
        Program {
            document: Arc::new(document.clone()),
            registry: Arc::clone(registry),
        },
        Bounds {
            charge: 10_000_000,
            memory: 16 * 1024 * 1024,
            call_depth: 100,
            live_tasks: 100,
            requests_per_park: 100,
            join_members: 100,
        },
        Start {
            target,
            args: vec![],
            bindings: Default::default(),
        },
    )
    .unwrap();
    let mut host = Observer::default();
    for _ in 0..1000 {
        match machine.run(&mut host, u64::MAX).unwrap() {
            Step::Slice => {}
            Step::Ended(end) => return (end, host.trace),
            Step::Parked(park) => {
                assert!(!park.requests.is_empty(), "deadlock");
                for request in park.requests.into_iter().rev() {
                    match request {
                        Request::Sleep(sleep) => {
                            host.trace.push(format!("sleep {:?}", sleep.duration));
                            machine.deliver(sleep.wait, Outcome::Elapsed).unwrap();
                        }
                        Request::Effect(effect) => {
                            host.trace.push(format!(
                                "effect {} {:?} {:?}",
                                effect.effect, effect.args, effect.identity
                            ));
                            let value = if effect.effect.as_str() == "projection" {
                                k::Datum::Handle(k::Handle {
                                    kind: "view".into(),
                                    id: "host-view".into(),
                                })
                            } else {
                                k::Datum::Record(vec![
                                    ("ok".into(), k::Datum::Bool(true)),
                                    ("ready".into(), k::Datum::Bool(false)),
                                ])
                            };
                            machine
                                .deliver(effect.wait, Outcome::Completed(value))
                                .unwrap();
                        }
                    }
                }
            }
        }
    }
    panic!("program failed to end");
}
fn document(body: &str) -> k::Document {
    k::parse_document(&format!("kernel 1\nnumbers by_spelling\n{body}")).unwrap()
}
fn roundtrip(document: &k::Document, registry: &Arc<k::FunctionRegistry>, library: &NamedLibrary) {
    let source = TypeScript.print(document, library).unwrap();
    let bindings = BTreeSet::new();
    let environment = Environment {
        library,
        effects: &document.manifest.effects,
        bindings: &bindings,
        functions: &std::collections::BTreeMap::new(),
    };
    let lowered =
        crate::lower(&source, &environment).unwrap_or_else(|error| panic!("{error}\n{source}"));
    // Retaining the tree also retains sites, task interleaving and errors at
    // bounds. The law's oracle remains a run on the production machine.
    assert_eq!(lowered.document, *document, "{source}");
    assert_eq!(
        run(document, registry, Target::Main),
        run(&lowered.document, registry, Target::Main),
        "{source}"
    );
    for entry in document.entries.keys() {
        assert_eq!(
            run(document, registry, Target::Entry(entry.clone())),
            run(&lowered.document, registry, Target::Entry(entry.clone()))
        );
    }
    let mut parser = crate::Parser::default();
    assert_eq!(
        parser.lower(&source, &environment).unwrap().document,
        *document
    );
}

#[test]
fn lowering_printed_documents_preserves_values_errors_mutations_and_effect_trace() {
    let mut registry = k::FunctionRegistry::new();
    lash_kernel_lib::register_text_json(&mut registry).unwrap();
    let helper = k::parse_definition("function test.wait(x: Any) -> Any\nkernel 1\ncharge 1\nbody {\n  do sleep 0.0\n  return x\n}\n").unwrap();
    let helper_id = registry.register(helper, None).unwrap();
    let registry = Arc::new(registry);
    let library = NamedLibrary::from_registry(&registry).unwrap();
    let concat = lash_kernel_dialect::Library::resolve(&library, "text.concat").unwrap();
    let cases = [
        document(
            r#"fn echo(x) { return x }
main {
  let values = (null, absent, true, 123456789012345678901234567890123456789, -0.0, nan, inf, -inf, b"ff00", &echo, "quote\\\"\n😀")
  finish values
}"#,
        ),
        document(
            r#"main {
  let xs = [1, 2]
  let alias = xs
  set xs[2] = 3
  remove xs[0]
  let r = {"x": xs, "missing": absent}
  set r.y = (4, 5)
  remove r.missing
  let m = map {"x": xs, 1: r}
  set m["new"] = alias
  remove m[1]
  let s = set {1, 2}
  set s[3] = true
  set s[1] = false
  print alias
  finish (r, m, s)
}"#,
        ),
        document(
            r#"main {
  let running = true
  while running {
    set running = false
    continue
  }
  while true { break }
  let xs = [true, false]
  let first = true
  for item in xs {
    if first { set xs[2] = true }
    set first = false
    print item
  }
  finish running
}"#,
        ),
        document(
            r#"fn throwing(x) { throw x }
main {
  let x = "before"
  let f = fn(v) { set x = v return x }
  let outcome = apply f("after")
  try {
    do call throwing(outcome)
  } catch caught {
    print caught
  } finally {
    print x
  }
  finish x
}"#,
        ),
        document(
            r#"fn worker(x) { print x do yield do sleep 0.0 return x }
main {
  let a = spawn call worker("a")
  let b = spawn call worker("b")
  let tasks = [a, b]
  let joined_all = join all tasks
  let again = join a
  let joined_settled = join settled tasks
  let joined_race = join race tasks
  let joined_any = join any tasks
  finish (joined_all, again, joined_settled, joined_race, joined_any)
}"#,
        ),
        document(
            r#"effect fetch(url: Text) -> Any
fn worker() {
  try { let page = perform fetch("cancel") as Any }
  finally { print "cleanup" do sleep 0.0 }
}
main {
  let task = spawn call worker()
  do cancel task
  try { let outcome = join task }
  catch caught { print "cancelled" }
  finish null
}"#,
        ),
        document(
            r#"main { let now = clock let random_value = random finish (now, random_value) }"#,
        ),
        document(r#"main { try { throw "error" } finally { print "finally" } }"#),
        document(r#"main { fail "failed" }"#),
        document(
            r#"effect projection() -> Handle("view")
main {
  let h = perform projection() as Handle("view")
  let answer = read(h, "lookup")
  finish answer
}"#,
        ),
        document(&format!(
            r#"use text.concat = @{concat}
main {{ try {{ let invalid = text.concat(true, "text") }} catch caught {{ finish caught }} }}"#
        )),
        document(&format!(
            r#"use text.concat = @{concat}
use test.wait = @{helper_id}
entry start() -> Any
private temp
fn start() {{ return "entry" }}
main {{
  let temp = text.concat("first", "second")
  let outcome = invoke test.wait(temp)
  finish outcome
}}"#
        )),
    ];
    for case in &cases {
        roundtrip(case, &registry, &library);
    }

    // The actual gate-3 edit vocabulary applied to its frontend fixture.
    let mut draft = Draft::open(
        document(
            r#"effect fetch(url: Text) -> Any
effect notify(text: Text) -> Any
main {
  let t1 = "https://a"
  let page = perform fetch(t1) as Any
  let t2 = page.ok
  if t2 { let t3 = "done" do perform notify(t3) as Any }
  finish page
}"#,
        ),
        None,
    )
    .unwrap();
    let mut environment = lash_kernel_check::Environment::new(&library);
    environment.effects = draft.document().manifest.effects.clone();
    let main = |path: &[u32]| k::Site::new(k::Unit::Main, path.to_vec());
    draft
        .apply(
            &Transaction {
                base: draft.identity(),
                edits: vec![
                    Edit::SetArgument {
                        action: main(&[1, 0]),
                        index: 0,
                        argument: k::Atom::Literal(k::Literal::Text("https://b".into())),
                    },
                    Edit::InsertStatement {
                        at: Position::before(main(&[3])).unwrap(),
                        statement: k::Stmt::Print {
                            value: k::Expr::Variable("page".into()),
                        },
                    },
                    Edit::SetCondition {
                        statement: main(&[3]),
                        condition: k::Expr::Member(Box::new(k::Member::Field {
                            target: k::Expr::Variable("page".into()),
                            field: "ready".into(),
                        })),
                    },
                ],
            },
            &environment,
        )
        .unwrap();
    roundtrip(draft.document(), &registry, &library);

    // Name hygiene, arbitrary text, source sizes and depths beyond ordinary TS.
    let unusual = k::Name::new("k\nreturn 😀");
    let mut huge = k::Expr::Literal(k::Literal::Text("x".repeat(70_000)));
    for _ in 0..30 {
        huge = k::Expr::Tuple(vec![huge]);
    }
    let case = k::Document::new(
        k::NumberPolicy::Float,
        vec![
            k::Stmt::Let {
                name: unusual.clone(),
                value: k::Rhs::Expr(huge),
            },
            k::Stmt::Finish {
                value: k::Expr::Variable(unusual),
            },
        ],
    );
    roundtrip(&case, &registry, &library);
}

#[test]
fn reserved_operations_obey_the_kernel_statement_rule() {
    let library = super::library();
    let effects = BTreeMap::new();
    let bindings = BTreeSet::new();
    let environment = Environment {
        library,
        effects: &effects,
        bindings: &bindings,
        functions: &std::collections::BTreeMap::new(),
    };
    let lowered = crate::lower(
        "let x = k.add(k.int(\"1\"), k.float(\"2.0\")); k.finish(k.tuple(x, k.absent));",
        &environment,
    )
    .unwrap();
    assert!(
        matches!(&lowered.document.main[0], k::Stmt::Let { value: k::Rhs::Expr(k::Expr::Call { function, .. }), .. } if Some(*function) == lash_kernel_dialect::Library::resolve(library, "num.add"))
    );
    for source in [
        "let x = k.tuple(k.yield());",
        "let x = k.sleep(k.add(k.int(\"1\"), k.int(\"2\")));",
        "let x = k.invoke(\"not-an-id\", []);",
        "let x = k.absent; k.unknown();",
        "let k = k.int(\"1\");",
        "let k = 1;",
    ] {
        assert!(crate::lower(source, &environment).is_err(), "{source}");
    }
    let deep = "k.tuple(".repeat(300) + "null" + &")".repeat(300);
    assert!(crate::lower(&format!("let x = {deep};"), &environment).is_err());
    // Kernel-looking text is ordinary text, never a parser mode switch.
    assert!(crate::lower("const text = 'k.yield()';", &environment).is_ok());
}
