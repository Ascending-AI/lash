//! TS-CALL-SLOTS: a stable direct call and its exported packed entry share
//! one body, preserving argument/default evaluation and parked state.
use super::machine;
use lash_kernel_doc::{
    Action, Atom, Datum, Expr, Handle, Integer, Literal, Name, Rhs, Stmt, Timestamp, Type,
};
use lash_kernel_vm::{
    Bounds, CellStart, End, Host, KernelMachine, Layout, Machine, Outcome, Program, Request, Step,
    Target,
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
    super::pages::start(
        program,
        BOUNDS,
        CellStart {
            target: Target::Main,
            args: Vec::new(),
            session: None,
            owner: super::pages::OWNER,
            modes: Default::default(),
            host: Vec::new(),
        },
    )
    .unwrap()
}

#[test]
fn direct_calls_charge_only_emitted_statements_atoms_and_body_expressions() {
    let lowered = super::lower("function f(a: number) { return a; } f(1); f(); f(1, 2);").unwrap();
    let mut machine = start(Program {
        release: super::pages::RELEASE,
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

/// Locate the source base case through any numeric admission branch.
fn returning_branch(stmts: &mut [Stmt]) -> Option<&mut Vec<Stmt>> {
    for stmt in stmts {
        if let Stmt::If {
            then_block,
            else_block,
            ..
        } = stmt
        {
            if matches!(then_block.as_slice(), [Stmt::Return { .. }]) {
                return Some(then_block);
            }
            if let Some(block) = returning_branch(then_block) {
                return Some(block);
            }
            if let Some(block) = returning_branch(else_block) {
                return Some(block);
            }
        }
    }
    None
}

#[test]
fn a_paged_positional_stack_resumes_with_a_new_layout() {
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
    let block = returning_branch(body).expect("the recursive base case");
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
        release: super::pages::RELEASE,
        document: Arc::new(lowered.document),
        library: machine::prepared().clone(),
    };
    let source = super::pages::Source::default();
    let mut machine = super::pages::with_source(
        program.clone(),
        BOUNDS,
        CellStart {
            owner: super::pages::OWNER,
            target: Target::Main,
            args: Vec::new(),
            session: None,
            modes: Default::default(),
            host: Vec::new(),
        },
        Layout::default(),
        &source,
    )
    .unwrap();
    let mut console = Console::default();
    let Step::Parked(park) = machine.run(&mut console, u64::MAX).unwrap() else {
        panic!("the deep call must park")
    };
    let sealed = machine.seal(super::pages::BUDGET).unwrap().unwrap();
    let heap = lash_kernel_state::paged::PagedHeap::open_header(
        sealed.root.heap,
        Box::new(source.clone()),
        lash_kernel_state::paged::EditEpoch(2),
    )
    .unwrap();
    let tasks =
        lash_kernel_state::paged::tree::Tree::<lash_kernel_state::paged::TaskRecord>::of_keyed(
            &sealed.root.tasks,
        );
    let task = tasks
        .get(
            &lash_kernel_state::paged::directory::ordinal_key(0),
            heap.pages(),
        )
        .unwrap()
        .unwrap();
    assert_eq!(task.frames.len(), 62);
    super::pages::reopen(&mut machine, &program, BOUNDS, &source, Layout(17));
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

/// What a session cell's end saved, recorded through the dialect's own
/// saving rules.
struct Saving {
    written: std::collections::BTreeMap<lash_kernel_doc::Site, lash_kernel_dialect::Written>,
    saved: std::rc::Rc<
        std::cell::RefCell<std::collections::BTreeMap<Name, lash_kernel_dialect::SavedFunction>>,
    >,
}
impl lash_kernel_vm::CellSave for Saving {
    fn save(
        &mut self,
        heap: &mut lash_kernel_state::paged::PagedHeap,
        cell: lash_kernel_vm::SaveCell<'_>,
    ) -> Result<lash_kernel_vm::SavedCell, lash_kernel_doc::heap::HeapFault> {
        let (kept, refused) = lash_kernel_dialect::save(
            heap,
            lash_kernel_dialect::Left {
                document: cell.document,
                bindings: cell.bindings,
                not_carried: cell.dropped,
                controls: &Default::default(),
                written: &self.written,
                values: &crate::function_values(),
                release: cell.release,
            },
            cell.held,
        )?;
        assert!(refused.is_empty(), "{refused:?}");
        self.saved
            .borrow_mut()
            .extend(kept.into_iter().map(|(name, kept)| (name, kept.function)));
        Ok(lash_kernel_vm::SavedCell::default())
    }
}

/// FIG-6137 (main's PC-FUNCTION law, on the heap): mutually recursive
/// functions whose direct calls enter private positional bodies are saved
/// as one bundle. Each forwards to a private body that calls the other's,
/// so each needs the other through the owners of the bodies it froze, and
/// both bundles hold the same two members; a function neither uses stays
/// out.
#[test]
fn mutually_recursive_positional_functions_save_as_one_bundle() {
    let lowered = super::lower(
        "function isEven(n: number): boolean { return n === 0 ? true : isOdd(n - 1); }\n\
         function isOdd(n: number): boolean { return n === 0 ? false : isEven(n - 1); }\n\
         const scale = (x: number) => x * 3;\n\
         console.log(isEven(4), isOdd(4), scale(2));",
    )
    .unwrap();
    let saved = std::rc::Rc::default();
    let mut machine = start(Program {
        release: super::pages::RELEASE,
        document: Arc::new(lowered.document),
        library: machine::prepared().clone(),
    });
    machine.set_cell_save(Box::new(Saving {
        written: lowered.written,
        saved: std::rc::Rc::clone(&saved),
    }));
    let mut console = Console::default();
    assert!(matches!(
        machine.run(&mut console, u64::MAX).unwrap(),
        Step::Ended(End::Finished(_))
    ));
    assert_eq!(console.lines.len(), 1);
    let saved = saved.borrow();
    let (even, odd) = (Name::new("isEven"), Name::new("isOdd"));
    assert!(saved[&even].needs().contains(&odd), "{:?}", saved[&even]);
    assert!(saved[&odd].needs().contains(&even), "{:?}", saved[&odd]);
    let members = |root: &Name| {
        lash_kernel_dialect::Bundle::of(&saved, root)
            .expect("a saved function")
            .members()
            .keys()
            .cloned()
            .collect::<Vec<_>>()
    };
    assert_eq!(members(&even), [even.clone(), odd.clone()]);
    assert_eq!(members(&odd), members(&even));
    assert_eq!(members(&Name::new("scale")), [Name::new("scale")]);
}
