use super::*;

pub(super) fn helper_compilation() {
    // The supplied call compiler must retain a guarded scalar helper's
    // source path before its body can be composed into a repeated region.
    for helper_body in [
        "return flat.add(a, b)",
        "if flat.lt(a, b) { return flat.add(a, b) } return flat.add(a, b)",
    ] {
        let helper = super::super::embedder::fragment(&format!(
            "function helper(a: Any, b: Any) -> Any\nkernel 1\nmetering statements\nbody {{ {helper_body} }}"
        ));
        let caller = scalar_document(
            "let i = 0 while flat.lt(i, 4) { set i = flat.identity(invoke helper(i, 1)) } return i",
        );
        let source = format!("definition \"{helper}\"\n{caller}");
        let embedder = Embedder::with(
            &source,
            Setup {
                probes: Some(scalar_probes),
                ..Setup::default()
            },
        );
        let (program, _) = embedder.program(true);
        let admitted = program
            .library
            .admit(&program.document, Default::default(), &program.selection)
            .unwrap_or_else(|_| panic!("helper source admitted"));
        let exe = admitted
            .executable(
                &program.document,
                &program.library,
                &program.documents,
                &program.functions,
                &program.instances,
            )
            .unwrap_or_else(|_| panic!("helper executable"));
        let caller = exe
            .islands
            .plans
            .iter()
            .find(|plan| {
                plan.body.statements.iter().any(|statement| {
                    matches!(statement.kind, crate::compile::islands::Kind::While { .. })
                })
            })
            .unwrap();
        let statement = caller
            .body
            .statements
            .iter()
            .find(|statement| matches!(statement.kind, crate::compile::islands::Kind::While { .. }))
            .unwrap();
        let crate::compile::islands::Kind::While { body, .. } = &statement.kind else {
            unreachable!()
        };
        let id = body.statements[0].id;
        let (stream, stmt, _) = exe.statement(id);
        let pc = (stmt.ops.start..stmt.ops.end())
            .find(|pc| {
                matches!(
                    stream.ops[*pc as usize],
                    lash_kernel_exec::Op::Nested { .. }
                )
            })
            .unwrap();
        let call =
            crate::compile::room::region::calls::VerifiedCall::compile(&exe, caller.code, id, pc);
        eprintln!(
            "loop helper {helper_body:?}: {:?}",
            call.as_ref().map(|_| "verified")
        );
        assert!(
            call.is_ok(),
            "a pure guarded helper must have source-call authority"
        );
    }
}
