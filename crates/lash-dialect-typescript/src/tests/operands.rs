//! OPERAND-ORDER: reads and raises precede later operands' statements.

use super::machine::{Ended, end, run};
use lash_kernel_doc::Datum;

#[test]
fn compound_assignment_reads_before_its_rhs_writes() {
    assert_eq!(
        end("const o = { n: 1 }; o.n += (o.n = 2); await control.finish(o.n);"),
        Ended::Finished(Datum::Float(3.0.into()))
    );
}

#[test]
fn array_reads_before_its_next_element_writes() {
    assert_eq!(
        end("const o = { x: 1 }; const r = [o.x, (o.x = 2)]; await control.finish(r);"),
        Ended::Finished(Datum::List(vec![
            Datum::Float(1.0.into()),
            Datum::Float(2.0.into())
        ]))
    );
}

#[test]
fn null_read_raises_before_the_next_operand_performs() {
    let result = run(
        "const n = null; function tool() { return host.echo('issued'); } [n.x, tool()];",
        &[],
    );
    assert!(result.end.starts_with("error type_error:"), "{result:?}");
    assert!(
        result.epochs.iter().all(|epoch| epoch.asked.is_empty()),
        "{result:?}"
    );
}

/// K-DOC-006: admitted source remains within the kernel's 64-level depth limit.
#[test]
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
        statements: vec![ast::Stmt::Expr(expr)],
        type_parameters: Default::default(),
        captions: Default::default(),
        comments: Vec::new(),
    };
    let environment_namespaces = lash_kernel_dialect::Namespaces::of_effects(
        &Default::default(),
        &lash_vocabulary::operations::ContractId::new("1"),
        &Default::default(),
    )
    .expect("the test's effects are operations");
    let environment =
        lash_kernel_dialect::Environment::new(super::library(), &environment_namespaces);
    let lowered = crate::lower::lower(
        &program,
        "",
        &environment,
        lash_kernel_dialect::Mode::Cell(lash_kernel_dialect::CellEnvironment::first()),
    )
    .unwrap();
    lash_kernel_doc::validate_document(&lowered.document, super::library()).unwrap();
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

/// ECMA-262 continue: a switch's synthetic block cannot consume loop continue.
#[test]
fn continue_leaves_only_the_switch() {
    assert_eq!(
        end(
            "var x = 0; while (x < 2) { switch (x) { case 0: x++; continue; } x++; } await control.finish(x);"
        ),
        Ended::Finished(Datum::Float(2.0.into()))
    );
}
