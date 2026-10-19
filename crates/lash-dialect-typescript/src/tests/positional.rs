//! TS-CALL-SLOTS: a stable direct call and its exported packed entry share
//! one body, preserving argument/default evaluation and parked state.
use super::machine;
use lash_kernel_doc::{Datum, Expr, Handle, Integer, Literal, Name, Rhs, Stmt, Timestamp, Type};
use lash_kernel_vm::{
    Bounds, CellStart, End, Host, KernelMachine, Machine, Outcome, Program, Request, Step, Target,
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
    memory_meter: lash_kernel_vm::MemoryMeter::On,
    charge: lash_kernel_vm::Charge::Exact,
    charge_limit: 10000000,
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
        saving: &mut lash_kernel_vm::CellSaveContext<'_>,
        cell: lash_kernel_vm::SaveCell<'_>,
    ) -> Result<lash_kernel_vm::SavedCell, lash_kernel_doc::heap::HeapFault> {
        let (kept, refused) = saving.save(
            lash_kernel_dialect::Left {
                document: cell.document,
                bindings: cell.bindings,
                not_carried: cell.dropped,
                controls: &Default::default(),
                written: &self.written,
                values: &crate::function_values(),
                release: cell.release,
                catalog: cell.catalog,
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

/// PC-FUNCTION / CONT-EXACT (FIG-6641, ruling 442039): saving one
/// mutually recursive function freezes its partner. It runs in a fresh
/// session with no partner binding, and cold recursion resumes exactly.
#[test]
fn a_saved_positional_function_runs_and_resumes_without_its_partner_in_scope() {
    let lowered = super::lower_with_effects(
        "function isEven(n: number): boolean { if (n === 0) return true; return isOdd(n - 1); }\n\
         function isOdd(n: number): boolean { if (n === 0) return false; return isEven(n - 1); }",
    )
    .unwrap();
    let written = lowered.written;
    let mut document = lowered.document.into_document();
    let echo = lash_kernel_doc::EffectName::new("echo").unwrap();
    document.manifest.effects.insert(
        echo.clone(),
        lash_kernel_doc::OperationRequirement::of_signature(machine::effects()[&echo].clone()),
    );
    let mut bodies = 0;
    for stmt in &mut document.main {
        if let Stmt::Assign {
            value: Rhs::Expr(Expr::Closure(closure)),
            ..
        } = stmt
            && let Some(block) = returning_branch(&mut closure.body)
            && let [Stmt::Return { value }] = block.as_slice()
        {
            let value = value.clone();
            let result = Name::new("parked_base_case");
            *block = vec![
                Stmt::Let {
                    name: result.clone(),
                    value: Rhs::Expr(Expr::Act(Box::new(lash_kernel_doc::ActExpr {
                        kind: lash_kernel_doc::ActKind::Perform {
                            effect: echo.clone(),
                            result: Type::Any,
                            then_yield: false,
                        },
                        args: vec![value],
                    }))),
                },
                Stmt::Return {
                    value: Expr::Variable(result),
                },
            ];
            // An atom-only whole RHS uses its canonical Action form.
            if let Stmt::Let { value: rhs, .. } = &mut block[0]
                && let Rhs::Expr(Expr::Act(action)) = rhs
                && let Some(action) = action.as_action()
            {
                *rhs = Rhs::Action(action);
            }
            bodies += 1;
        }
    }
    assert_eq!(bodies, 2, "both recursive base cases park");
    let saved = std::rc::Rc::default();
    let mut original = start(Program {
        release: super::pages::RELEASE,
        document: Arc::new(document),
        library: machine::prepared().clone(),
    });
    original.set_options(lash_kernel_vm::MachineOptions {
        cell_save: Some(Box::new(Saving {
            written,
            saved: std::rc::Rc::clone(&saved),
        })),
        ..Default::default()
    });
    assert!(matches!(
        original.run(&mut Console::default(), u64::MAX).unwrap(),
        Step::Ended(End::Finished(_))
    ));
    let saved = saved.borrow();
    let even = &saved[&Name::new("isEven")];
    let mut standalone = even.document.clone();
    for n in [0.0, 1.0, 4.0, 5.0] {
        standalone.main.push(Stmt::Print {
            value: Expr::Act(Box::new(lash_kernel_doc::ActExpr {
                kind: lash_kernel_doc::ActKind::Call(lash_kernel_doc::Callee::Declared(
                    even.name.clone(),
                )),
                args: vec![
                    Expr::Literal(Literal::Absent),
                    Expr::List(vec![Expr::Literal(Literal::Float(
                        lash_kernel_doc::Float::new(n),
                    ))]),
                ],
            })),
        });
    }
    let program = Program {
        release: super::pages::RELEASE,
        document: Arc::new(standalone),
        library: machine::prepared().clone(),
    };
    let run = |cold: bool| {
        let source = super::pages::Source::default();
        let mut current = super::pages::with_source(
            program.clone(),
            BOUNDS,
            CellStart {
                owner: super::pages::OWNER,
                target: Target::Main,
                args: vec![],
                session: None,
                modes: Default::default(),
                host: vec![],
            },
            &source,
        )
        .unwrap();
        let mut console = Console::default();
        let mut waits = 0;
        loop {
            let step = current
                .run(&mut console, if cold { 1 } else { u64::MAX })
                .unwrap();
            if cold && matches!(step, Step::Slice | Step::Parked(_)) {
                super::pages::reopen(&mut current, &program, BOUNDS, &source);
            }
            match step {
                Step::Slice => {}
                Step::Parked(park) => {
                    for request in park.requests {
                        let Request::Effect(effect) = request else {
                            panic!("the recursive base effect")
                        };
                        waits += 1;
                        current
                            .deliver(effect.wait, Outcome::Completed(effect.args[0].clone()))
                            .unwrap();
                    }
                }
                Step::Ended(End::Finished(_)) => break,
                other => panic!("{other:?}"),
            }
        }
        (
            console.lines,
            current.meters().charged.expect("exact charge"),
            waits,
        )
    };
    let plain = run(false);
    assert_eq!(plain, run(true));
    assert_eq!(
        plain.0,
        vec![
            Datum::Bool(true),
            Datum::Bool(false),
            Datum::Bool(true),
            Datum::Bool(false)
        ]
    );
    assert_eq!(plain.2, 4);
}

/// TS-CALL-SLOTS, ECMA-262 Array callbacks: fixed slots preserve defaults,
/// receiver and operand order; rest and arguments retain the complete list.
#[test]
fn array_callback_slots_agree_with_packed_calls() {
    for (callback, packed) in [
        ("(a, b) => a - b", false),
        ("function(a, b) { return a - b; }", false),
        ("(a, b = 4, c = 9, d = 7) => a - b + (d - 7)", false),
        ("function(a, ...rest) { return a - rest[0]; }", true),
        (
            "function(a, b) { return arguments[0] - arguments[1]; }",
            true,
        ),
    ] {
        let direct = format!("console.log([3, 1, 2].sort({callback}).join(','));");
        let generic = format!(
            "function call(xs, f) {{ return xs.sort(f); }} console.log(call([3, 1, 2], {callback}).join(','));"
        );
        let text = super::main_text(&direct);
        assert_eq!(text.contains("ts.pad"), packed, "{callback}\n{text}");
        let direct = machine::run(&direct, &[]);
        let generic = machine::run(&generic, &[]);
        assert_eq!(direct.end, "ok");
        assert_eq!(direct.end, generic.end);
        assert_eq!(direct.lines(), ["1,2,3"]);
        assert_eq!(direct.lines(), generic.lines());
    }
    for method in ["map", "filter"] {
        for (callback, packed) in [
            (
                "(value, index, receiver, missing = 7) => value + index + receiver.length + missing",
                false,
            ),
            (
                "function(value, index, receiver) { return value + index + receiver.length + this.offset; }",
                false,
            ),
            (
                "function(value, ...rest) { return value + rest[0] + rest[1].length; }",
                true,
            ),
            (
                "function(value) { return arguments.length + value + arguments[1] + arguments[2].length; }",
                true,
            ),
        ] {
            let direct =
                format!("console.log([1, , 3].{method}({callback}, {{offset: 5}}).join(','));");
            let generic = format!(
                "function call(xs, f, context) {{ return xs.{method}(f, context); }} console.log(call([1, , 3], {callback}, {{offset: 5}}).join(','));"
            );
            let text = super::main_text(&direct);
            assert_eq!(text.contains("ts.pad"), packed, "{callback}\n{text}");
            let direct = machine::run(&direct, &[]);
            let generic = machine::run(&generic, &[]);
            assert_eq!(direct.end, "ok");
            assert_eq!(direct.end, generic.end);
            assert_eq!(direct.lines(), generic.lines());
        }
    }
    for (source, expected) in [
        (
            "function compare(a, b) { return a - b; } const alias = compare; console.log([3, 1, 2].sort(compare).join(','), [3, 1, 2].sort(alias).join(','));",
            "1,2,3 1,2,3",
        ),
        (
            "function first(value) { return value; } const alias = first; console.log([1, 2].map(first).join(','), [1, 2].map(alias).join(','));",
            "1,2 1,2",
        ),
        (
            "console.log([3, 1, 2].map((value, index, receiver, {offset} = {offset: 7}) => value + index + receiver.length + offset).join(','));",
            "13,12,14",
        ),
        (
            "function run() { return [1, 2].map(value => value + this.offset).join(','); } console.log(run.call({offset: 5}));",
            "6,7",
        ),
        (
            "const xs = [1, 2]; function context() { xs.push(3); return {offset: 5}; } console.log(xs.map(function(value) { return value + this.offset; }, context()).join(','));",
            "6,7,8",
        ),
    ] {
        let recorded = machine::run(source, &[]);
        assert_eq!(recorded.end, "ok", "{source}");
        assert_eq!(recorded.lines(), [expected], "{source}");
    }
}
