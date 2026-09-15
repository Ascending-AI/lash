//! FIG-2998: captures by value and the mutable-capture refusal for lifted
//! process literals.

use lash_typescript::Diagnostic;

/// FIG-2998: a `const`-bound arrow the program never calls is a raised
/// process literal, and a cell local its body reads becomes a hidden start
/// argument carrying the value the variable had when the process started.
/// A mutable capture refuses, naming the variable and the rewrite.
#[test]
fn a_lifted_body_captures_immutable_cell_locals_and_refuses_mutable_ones() {
    let constant = r#"
        const budget = 40;
        const spend = async (tick: unknown) => {
            console.log(budget);
            return budget;
        };
    "#;
    // The lift is linker-owned: the lowered program carries the literal with
    // the reads it wants as hidden start arguments; the hoisted declaration
    // appears at link.
    let program = lash_typescript::parse(constant).expect("an immutable capture lifts");
    assert!(
        program.declarations.is_empty(),
        "the front end hoists nothing"
    );
    let literal = program
        .main
        .children()
        .find_map(|child| match child {
            lashlang::Expr::Assign { expr, .. } => match expr.as_ref() {
                lashlang::Expr::ProcessLiteral(literal) => Some(literal),
                _ => None,
            },
            found => panic!("expected the const-bound literal, got {found:?}"),
        })
        .expect("the const-bound arrow lowers as a process literal");
    assert!(
        literal
            .hidden_args
            .iter()
            .any(|arg| arg.name.as_str() == "budget"),
        "the read becomes a hidden start argument: {:?}",
        literal.hidden_args
    );

    let mutable = r#"
        let counter = 1;
        const tickle = async (tick: unknown) => {
            console.log(counter);
            return counter;
        };
        const handle = start(tickle, { tick: 1 });
        finish(handle);
    "#;
    let Diagnostic { message, .. } = lash_typescript::parse(mutable).expect_err("mutable refused");
    assert!(
        message.contains("`counter`"),
        "the diagnostic names the variable: {message}"
    );
    assert!(
        message.contains("pass the value in as a start argument")
            || message.contains("pass the value to the process through its `run` arguments"),
        "{message}"
    );
}

/// FIG-2998: a `defineProcess` binding registers the signal set its own body
/// waits for. The declaration key is gone, so the wait sites are the only
/// source left; a `defineProcess` lowers straight to a process declaration and
/// never reaches the linker's lifted-literal inference, so an empty set here
/// means the process refuses every signal it was written to receive
/// ("emitted undeclared event type `signal.first`") at runtime.
#[test]
fn a_define_process_binding_registers_the_signals_its_body_waits_for() {
    let source = r#"
        const waiter = defineProcess({
            name: "waiter",
            run: async (workflow_id) => {
                const first = await waitSignal("first");
                if (first === "skip") {
                    await waitSignal("unreached");
                }
                const second = await waitSignal("second");
                return { workflow_id: workflow_id, first: first, second: second };
            }
        });
        const handle = start(waiter, { workflow_id: "w" });
        finish({ process_id: handle.process_id });
    "#;
    let program = lash_typescript::parse(source).expect("the waiter program links");
    let signals = program
        .declarations
        .iter()
        .find_map(|declaration| match declaration {
            lashlang::Declaration::Process(process) if process.name.as_str() == "waiter" => {
                Some(process.signals.clone())
            }
            _ => None,
        })
        .expect("the waiter declaration is registered");
    let names = signals
        .iter()
        .map(|signal| signal.name.to_string())
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        vec![
            "first".to_string(),
            "second".to_string(),
            "unreached".to_string()
        ],
        "every literal wait site declares its signal, including the branch this run never takes"
    );
}
