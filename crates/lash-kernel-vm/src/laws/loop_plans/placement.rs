//! Same-price values cannot substitute another physical binding at entry.
use super::*;
use crate::compile::room::{Phase, Point, placement::BindingPlacement};
use lash_kernel_exec::Op;

pub(super) fn binding_values() {
    for layout in [0, 1, 42, u64::MAX] {
        let text = super::super::layout::scalar_document(
            "let left = 3 let right = 8 set left = right return left",
        );
        let run = Embedder::with(
            &text,
            Setup {
                layout: crate::Layout(layout),
                ..Setup::default()
            },
        );
        let (program, _) = run.program(true);
        let admitted = program
            .library
            .admit(&program.document, Default::default(), &program.selection)
            .unwrap_or_else(|_| panic!("placement source is admitted"));
        let exe = admitted
            .executable(
                &program.document,
                &program.library,
                &program.documents,
                &program.functions,
                &program.instances,
            )
            .unwrap_or_else(|_| panic!("placement source compiles"));
        let body = (0..exe.code_count())
            .map(|n| crate::compile::CodeId(n as u32))
            .find(|code| {
                exe.code(*code).params.is_empty()
                    && exe.block(exe.code(*code).body).stmts.len() == 4
            })
            .unwrap();
        let placement = BindingPlacement::compile(&exe, body).unwrap();
        let id = exe.block(exe.code(body).body).stmts[2];
        let (stream, statement, _) = exe.statement(id);
        let (pc, local) = (statement.ops.start..statement.ops.end())
            .find_map(|pc| match stream.ops[pc as usize] {
                Op::Local { local, .. } => Some((pc, local)),
                _ => None,
            })
            .unwrap();
        let read = Point {
            site: (**stream.source_sites[pc as usize].as_ref().unwrap()).clone(),
            phase: Phase::Commit,
        };
        assert_eq!(placement.verify_read(&read, local.at), Ok(()));
        let wrong = (0..exe.code(body).slots.len() as u32)
            .find(|at| *at != local.at)
            .unwrap();
        assert!(
            placement.verify_read(&read, wrong).is_err(),
            "equal scalar prices do not authorize the other lexical binding"
        );
    }
}
