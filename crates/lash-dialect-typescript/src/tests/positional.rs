//! TS-CALL-SLOTS: a stable direct call and its exported packed entry share
//! one body, preserving argument/default evaluation and parked state.
use super::machine;
use lash_kernel_doc::{
    Action, Atom, Datum, Expr, Handle, Integer, Literal, Name, Rhs, Stmt, Timestamp, Type,
};
use lash_kernel_vm::{
    Bindings, Bounds, End, Host, KernelMachine, Layout, Machine, Outcome, Program, Request, Start,
    Step, Target,
};
use std::sync::Arc;

#[test]
fn positional_and_packed_entries_agree_on_missing_extra_and_default_arguments() {
    let recorded = machine::run(
        r#"
      function f(a = 7, b = 9) { return a + ":" + b; }
      const g = f;
      console.log(f(), g(), f(undefined, 3), g(undefined, 3), f(1, 2, 99), g(1, 2, 99));
      function captured(a = 1, read = () => a) { a = 2; return read(); }
      const h = captured;
      console.log(captured(), h());
      console.log(f.length, f.name);
    "#,
        &[],
    );
    assert_eq!(recorded.end, "ok");
    assert_eq!(recorded.lines(), ["7:9 7:9 7:3 7:3 1:2 1:2", "2 2", "0 f"]);
}

#[test]
fn extra_arguments_evaluate_in_order_after_retained_arguments_are_copied() {
    let recorded = machine::run(
        r#"
      let n = 0; let log = "";
      function take(a, b) { return a + ":" + b; }
      function bump(tag) { log += tag; return ++n; }
      console.log(take(n, bump("B"), bump("X")), log, n);
      const g = take; n = 0; log = "";
      console.log(g(n, bump("B"), bump("X")), log, n);
    "#,
        &[],
    );
    assert_eq!(recorded.end, "ok");
    assert_eq!(recorded.lines(), ["0:1 BX 2", "0:1 BX 2"]);
}

#[test]
fn arguments_rest_and_reassigned_functions_keep_their_packed_entry() {
    let recorded = machine::run(
        r#"
      function args(a) { return arguments.length + ":" + arguments[1]; }
      function rest(a, ...tail) { return tail.join(":"); }
      function changed(a) { return a; }
      changed = (a, b) => b;
      console.log(args(1, 2), rest(1, 2, 3), changed(1, 8));
      function arrow(a) { const read = () => arguments[1]; return read(); }
      console.log(arrow(1, 9));
      function outer(a) { function same(a) { return a + 1; } return same(a); }
      console.log(outer(4));
    "#,
        &[],
    );
    assert_eq!(recorded.end, "ok");
    assert_eq!(recorded.lines(), ["2:2 2:3 8", "9", "5"]);
}

#[derive(Default)]
struct Console {
    lines: Vec<Datum>,
}
impl Host for Console {
    fn clock(&mut self) -> Timestamp {
        Timestamp {
            nanoseconds: Integer::from(0),
        }
    }
    fn random(&mut self) -> u64 {
        0
    }
    fn read(&mut self, _: &Handle, _: &Datum) -> Result<Datum, lash_kernel_doc::ErrorDatum> {
        unreachable!("these laws have no host read")
    }
    fn print(&mut self, value: &Datum) {
        self.lines.push(value.clone());
    }
    fn cancel_requested(&mut self) -> bool {
        false
    }
}
const BOUNDS: Bounds = Bounds {
    charge: 10000000,
    memory: 64 << 20,
    call_depth: 200,
    live_tasks: 100,
    requests_per_park: 100,
    join_members: 100,
};
fn start(program: Program) -> KernelMachine {
    KernelMachine::start(
        program,
        BOUNDS,
        Start {
            target: Target::Main,
            args: Vec::new(),
            bindings: Bindings::default(),
        },
    )
    .unwrap()
}

#[test]
fn direct_calls_charge_only_emitted_statements_atoms_and_body_expressions() {
    let lowered = super::lower("function f(a: number) { return a; } f(1); f(); f(1, 2);").unwrap();
    let mut machine = start(Program {
        document: Arc::new(lowered.document),
        library: machine::prepared().clone(),
    });
    assert!(matches!(
        machine.run(&mut Console::default(), u64::MAX).unwrap(),
        Step::Ended(End::Finished(_))
    ));
    // Private entry declaration: 2. Body closure and packed closure: 2 each.
    // Function token: statement + tuple node + six nodes + six writes = 14.
    // Calls: statement + receiver/argument atoms + return statement/variable.
    assert_eq!(machine.meters().charged, 2 + 2 + 2 + 14 + 5 + 4 + 5);
}

#[test]
fn a_positional_body_parked_at_sixty_recursive_calls_round_trips_exactly() {
    let mut lowered = super::lower_with_effects("function descend(n) { if (n < 1) return n; return descend(n - 1); } console.log(descend(60));").unwrap();
    // A host may edit the kernel document. Insert the wait at the base case
    // so the synchronous positional entry itself parks with the whole stack.
    let body = lowered
        .document
        .main
        .iter_mut()
        .find_map(|stmt| match stmt {
            Stmt::Assign {
                value: Rhs::Expr(Expr::Closure(closure)),
                ..
            } => Some(&mut closure.body),
            _ => None,
        })
        .expect("the shared positional body");
    let block = body
        .iter_mut()
        .find_map(|stmt| match stmt {
            Stmt::If { then_block, .. } => Some(then_block),
            _ => None,
        })
        .unwrap();
    lowered.document.manifest.effects.insert(
        lash_kernel_doc::EffectName::new("echo").unwrap(),
        machine::effects()[&lash_kernel_doc::EffectName::new("echo").unwrap()].clone(),
    );
    let result = Name::new("parked_answer");
    *block = vec![
        Stmt::Let {
            name: result.clone(),
            value: Rhs::Action(Action::Perform {
                effect: lash_kernel_doc::EffectName::new("echo").unwrap(),
                args: vec![Atom::Literal(Literal::Float(lash_kernel_doc::Float::new(
                    0.0,
                )))],
                result: Type::Any,
            }),
        },
        Stmt::Return {
            value: Expr::Variable(result),
        },
    ];
    let program = Program {
        document: Arc::new(lowered.document),
        library: machine::prepared().clone(),
    };
    let mut machine = start(program.clone());
    let mut console = Console::default();
    let Step::Parked(park) = machine.run(&mut console, u64::MAX).unwrap() else {
        panic!("the deep call must park")
    };
    let before = machine.export().unwrap();
    assert_eq!(before.tasks[0].calls.len(), 62);
    let bytes = serde_json::to_vec(&before).unwrap();
    machine = KernelMachine::import_with_layout(program, BOUNDS, before, Layout(17)).unwrap();
    assert_eq!(
        serde_json::to_vec(&machine.export().unwrap()).unwrap(),
        bytes
    );
    let [Request::Effect(effect)] = park.requests.as_slice() else {
        panic!("one base-case effect")
    };
    machine
        .deliver(
            effect.wait,
            Outcome::Completed(Datum::Float(lash_kernel_doc::Float::new(0.0))),
        )
        .unwrap();
    assert!(matches!(
        machine.run(&mut console, u64::MAX).unwrap(),
        Step::Ended(End::Finished(_))
    ));
    assert_eq!(console.lines, [Datum::Text("0".into())]);
}
