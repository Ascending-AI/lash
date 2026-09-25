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
        const handle = await processes.start({ definition: tickle, args: { tick: 1 } });
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

/// FIG-2998 / FIG-2999: a process registers the signal set its own body waits
/// for. The `signals` declaration key is gone and so is `defineProcess`, so a
/// process is a lifted literal and the wait sites in its body are the only
/// source of its signal set; an empty set here means the process refuses every
/// signal it was written to receive ("emitted undeclared event type
/// `signal.first`") at runtime. The inference is the linker's, over the body
/// of the literal it lifts, and it counts a wait in a branch this run never
/// takes.
#[test]
fn a_lifted_process_registers_the_signals_its_body_waits_for() {
    let source = r#"
        const waiter = async (workflow_id: string) => {
            const first = await waitSignal("first");
            if (first === "skip") {
                await waitSignal("unreached");
            }
            const second = await waitSignal("second");
            return { workflow_id: workflow_id, first: first, second: second };
        };
        const handle = await processes.start({ definition: waiter, args: { workflow_id: "w" } });
        finish(handle);
    "#;
    let linked = lash_typescript::link(source, &process_environment())
        .unwrap_or_else(|error| panic!("the waiter program links: {error:?}"));
    let signals = linked
        .artifact
        .ir()
        .declarations
        .iter()
        .find_map(|declaration| match declaration {
            lashlang::Declaration::Process(process) => Some(process.signals.clone()),
            _ => None,
        })
        .expect("the lifted waiter declaration is registered");
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

fn process_environment() -> lashlang::LashlangHostEnvironment {
    let mut catalog = lashlang::LashlangHostCatalog::new();
    // FIG-2999: `start` is a leaf tool whose `definition` slot is typed as a
    // process, and that expected type is what lifts the literal.
    catalog
        .add_module_operation(
            ["processes"],
            "Processes",
            "start",
            "processes.start",
            lashlang::TypeExpr::Object(vec![lashlang::TypeField {
                name: "definition".into(),
                ty: lashlang::TypeExpr::Process(lashlang::ProcessType::unknown()),
                optional: false,
            }]),
            lashlang::TypeExpr::Any,
        )
        .expect("processes.start binding");
    lashlang::LashlangHostEnvironment::new(catalog, lashlang::LashlangAbilities::default())
}

/// FIG-3707: inside a lifted body, a closure shares the body's own bindings
/// (a binding cell the body's frame holds); only a capture of the starting
/// cell's mutable bindings still refuses, since the process sees the values
/// it was started with.
#[test]
fn a_lifted_body_shares_its_own_bindings_with_its_closures() {
    let source = r#"
        const tally = async (tick: unknown) => {
            let n = 0;
            [1, 2, 3].forEach((x) => { n += x; });
            return n;
        };
    "#;
    let program = lash_typescript::parse(source)
        .expect("a closure inside a process body may assign the body's own binding");
    let text = format!("{:?}", program.main);
    assert!(
        text.contains("__typescript_cell_new"),
        "the body's binding lives in a cell its closure shares: {text}"
    );
}
