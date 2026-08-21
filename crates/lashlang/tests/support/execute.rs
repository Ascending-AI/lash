use lashlang::{ExecutionHost, ExecutionOutcome, State, parse};

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum ExecuteError {
    #[error(transparent)]
    Parse(#[from] lashlang::ParseError),
    #[error(transparent)]
    Link(#[from] lashlang::LinkError),
    #[error(transparent)]
    Runtime(#[from] lashlang::RuntimeError),
}

pub async fn execute<H: ExecutionHost>(
    source: &str,
    state: &mut State,
    host: &H,
    environment: lashlang::LashlangHostEnvironment,
) -> Result<ExecutionOutcome, ExecuteError> {
    let program = parse(source)?;
    let compiled = if let Ok(linked) = lashlang::LinkedModule::link(program.clone(), &environment) {
        lashlang::compile_linked(&linked)
    } else if program_contains_start_process(&program.main) {
        let linked = lashlang::LinkedModule::link(program, &environment)?;
        lashlang::compile_linked(&linked)
    } else {
        lashlang::compile(source)?
    };
    lashlang::execute(&compiled, state, host)
        .await
        .map_err(ExecuteError::Runtime)
}

pub fn program_contains_start_process(expr: &lashlang::Expr) -> bool {
    struct Finder(bool);

    impl lashlang::ExprVisitor for Finder {
        fn visit_expr(&mut self, expr: &lashlang::Expr) {
            if matches!(expr, lashlang::Expr::StartProcess(_)) {
                self.0 = true;
                return;
            }
            lashlang::walk_expr(self, expr);
        }
    }

    let mut finder = Finder(false);
    lashlang::ExprVisitor::visit_expr(&mut finder, expr);
    finder.0
}
