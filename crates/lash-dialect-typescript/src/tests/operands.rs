//! OPERAND-ORDER: reads and raises precede later operands' statements.

use super::machine::{Ended, end};
use lash_kernel_doc::Datum;

#[test]
fn compound_assignment_reads_before_its_rhs_writes() {
    assert_eq!(
        end("const o = { n: 1 }; o.n += (o.n = 2); await control.finish(o.n);"),
        Ended::Finished(Datum::Float(3.0.into()))
    );
}

/// K-DOC-006: admitted source remains within the kernel's 64-level depth limit.
fn native_expression_depth_respects_the_document_limit() {
    use crate::adapter as ast;
    // Exercise the lowerer independently of the parser's stricter source limit.
    let expr = (0..90).fold(ast::Expr::Number(1.0), |left, _| ast::Expr::Binary {
        left: Box::new(left),
        op: ast::BinaryOp::Add,
        right: Box::new(ast::Expr::Number(1.0)),
        operand_spans: None,
    });
    let program = ast::Program {
        imported: Default::default(),
        export_aliases: Default::default(),
        statements: vec![ast::Stmt::Expr(expr)],
        annotation_limits: (64, 4096),
        captions: Default::default(),
        comments: Vec::new(),
    };
    let environment_namespaces =
        crate::tests::linked::namespaces(&Default::default(), &Default::default())
            .expect("the test's effects are operations");
    let environment =
        lash_kernel_dialect::Environment::new(super::library(), &environment_namespaces);
    let lowered = crate::lower::lower(
        &program,
        "",
        &environment,
        lash_kernel_dialect::Mode::Cell(lash_kernel_dialect::CellEnvironment::first()),
        lash_kernel_dialect::workflow::ImplicitTextConversionPolicy::Allow,
    )
    .unwrap();
    lash_kernel_doc::validate_document(&lowered.document, super::library()).unwrap();
}

/// FIG-7353: binary lowering at the depth-law limit fits a default thread stack.
#[test]
fn native_expression_depth_fits_the_default_stack() {
    std::thread::spawn(native_expression_depth_respects_the_document_limit)
        .join()
        .expect("binary lowering fits the default thread stack");
}

/// OPERAND-ORDER covers the other consumers of the same operand buffer.
#[test]
fn reads_precede_later_statements_in_constructions_and_updates() {
    for (source, expected) in [
        (
            "const o = {x: 1}; const r = [o.x, , (o.x = 2)]; await control.finish(r[0]);",
            1.0,
        ),
        (
            "const o = {x: 1}; const r = [o.x, ...[o.x = 2]]; await control.finish(r[0]);",
            1.0,
        ),
        (
            "const o = {x: 1}; const r = {a: o.x, b: (o.x = 2)}; await control.finish(r.a);",
            1.0,
        ),
        (
            "const o = {x: 1}; const r = o.x++; await control.finish(r);",
            1.0,
        ),
        (
            "const o = {x: 1}; const r = ++o.x; await control.finish(r);",
            2.0,
        ),
        (
            "const a = [1]; a.push(a.length, a.length); await control.finish(a[2]);",
            1.0,
        ),
        (
            "let x = 1; const r = x + (x = 2); await control.finish(r);",
            3.0,
        ),
        (
            "const a = [0, 0]; let index = 0; a[index] = (index = 1); await control.finish(a[0]);",
            1.0,
        ),
    ] {
        assert_eq!(
            end(source),
            Ended::Finished(Datum::Float(expected.into())),
            "{source}"
        );
    }
    assert_eq!(
        end("const o = {x: 'a'}; const r = `${o.x}${o.x = 'b'}`; await control.finish(r);"),
        Ended::Finished(Datum::Text("ab".into()))
    );
    // Evaluating an argument precedes checking whether the callee is callable.
    assert_eq!(
        end(
            "try { (1 as any)(('x' as number) + 1); } catch (e) { await control.finish(e.message); }"
        ),
        end(
            "try { const r = ('x' as number) + 1; } catch (e) { await control.finish(e.message); }"
        )
    );
    // Both operand values are read before a trusted numeric coercion can fail.
    assert_eq!(
        end(
            "try { const r = ('x' as number) + ((!(123 as boolean)) as number); } catch (e) { await control.finish(e.message); }"
        ),
        end("try { const r = !(123 as boolean); } catch (e) { await control.finish(e.message); }")
    );
}

/// Switch guards and destructuring introduce private declarations. At the
/// bytecode local boundary they must keep every declaration or refuse the start.
#[test]
fn switch_and_destructuring_locals_obey_the_bytecode_limit() {
    use lash_kernel_doc::{Expr, Literal, Rhs, Stmt};
    use lash_kernel_value::Name;
    use lash_kernel_vm::{
        CellStart, End, Host, KernelMachine, Machine, Program, StartError, Step, Target,
    };
    use std::collections::BTreeMap;

    // Count source declarations independently of the bytecode allocator. These
    // fixtures contain only straight-line declarations, branches and switch's
    // one-pass while, whose pass counter is its single hidden local.
    fn declarations(block: &[Stmt]) -> usize {
        block
            .iter()
            .map(|stmt| match stmt {
                Stmt::Let { .. } => 1,
                Stmt::If {
                    then_block,
                    else_block,
                    ..
                } => declarations(then_block) + declarations(else_block),
                Stmt::While {
                    body, else_block, ..
                } => 1 + declarations(body) + declarations(else_block),
                Stmt::For { .. } | Stmt::Try(_) | Stmt::ReturnVia { .. } => {
                    panic!("unexpected hidden-slot form in boundary fixture")
                }
                _ => 0,
            })
            .sum()
    }
    struct Passive;
    impl Host for Passive {
        fn print(&mut self, _: &Datum) {}
        fn cancel_requested(&mut self) -> bool {
            false
        }
    }
    let cases = (0..128)
        .map(|n| format!("case {n}: break;"))
        .collect::<String>();
    let fields = (0..128)
        .map(|n| format!("f{n}: v{n}"))
        .collect::<Vec<_>>()
        .join(",");
    for source in [
        format!("switch (0) {{ {cases} }}"),
        format!("{{ const {{{fields}}} = {{}}; }}"),
    ] {
        let lowered = super::lower(&source).expect("source lowers");
        let original = lowered.document.shared().as_ref();
        let body = original.main.as_ref().expect("a cell has main");
        let used = declarations(body);
        assert!(used > 1 && used < 65535);
        for extra in [0, 1] {
            let mut document = original.clone();
            let mut block: Vec<_> = (0..65535 - used + extra)
                .map(|n| Stmt::Let {
                    name: Name::new(format!("boundary_padding_{n}")),
                    value: Rhs::Expr(Expr::Literal(Literal::Null)),
                })
                .collect();
            block.extend(body.clone());
            // The source cell falls through; observe that all its lowered
            // statements execute before this fixture's explicit result.
            block.push(Stmt::Return {
                value: Expr::Literal(Literal::Bool(true)),
            });
            document.main = Some(block);
            let library = super::machine::prepared().clone();
            let (content, root) = super::linked::content(&document, library.helpers().as_ref());
            let program = Program::prepare(
                root,
                content.documents,
                &content.links,
                super::pages::CLOSURE,
                library,
            )
            .expect("validator admits the document");
            let values = super::pages::Session::default();
            let start = CellStart {
                owner: super::pages::OWNER,
                target: Target::Main,
                args: vec![],
                session: values.root,
                modes: BTreeMap::new(),
                host: vec![],
            };
            let bounds = lash_kernel_vm::Bounds {
                memory: 1 << 30,
                charge_limit: 10_000_000,
                memory_meter: lash_kernel_vm::MemoryMeter::On,
                charge: lash_kernel_vm::Charge::Exact,
                call_depth: lash_kernel_vm::Bounds::DEFAULT_CALL_DEPTH,
                live_tasks: 100,
                requests_per_park: 100,
                join_members: 100,
            };
            let result = super::pages::with_source(program, bounds, start, &values.source);
            if extra == 1 {
                match result {
                    Err(StartError::Linked { problem }) => assert!(
                        problem.contains("parameters and locals")
                            && problem.contains("limit of 65535"),
                        "{problem}"
                    ),
                    Err(other) => panic!("unexpected refusal: {other:?}"),
                    Ok(_) => panic!("overlimit source declarations compiled"),
                }
                continue;
            }
            let mut machine: KernelMachine = result.expect("65535 total locals compile");
            loop {
                match machine.run(&mut Passive, 10_000_000).unwrap() {
                    Step::Slice => {}
                    Step::Ended(End::Finished(done)) => {
                        assert_eq!(done.result, Datum::Bool(true));
                        break;
                    }
                    other => panic!("boundary source did not execute: {other:?}"),
                }
            }
        }
    }
}
