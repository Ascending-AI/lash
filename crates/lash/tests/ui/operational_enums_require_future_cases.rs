fn send(outcome: lash::SendOutcome) {
    match outcome {
        lash::SendOutcome::OperationSettled { .. }
        | lash::SendOutcome::Settled { .. }
        | lash::SendOutcome::Parked { .. }
        | lash::SendOutcome::Stalled { .. }
        | lash::SendOutcome::Refused { .. }
        | lash::SendOutcome::Withdrawn { .. }
        | lash::SendOutcome::Unknown { .. } => {}
    }
}

fn build(error: lash::durable::DurableBuildError) {
    use lash::durable::DurableBuildError;
    match error {
        DurableBuildError::DuplicateEngine { .. }
        | DurableBuildError::EngineFormatMismatch { .. }
        | DurableBuildError::DuplicateProvider { .. }
        | DurableBuildError::HelperReleaseCatalog { .. }
        | DurableBuildError::InvalidConfig(_)
        | DurableBuildError::SettingsOwnedByHost => {}
    }
}

fn diagnostic(error: lash::plugins::diagnostics::DiagnosticError) {
    use lash::plugins::diagnostics::DiagnosticError;
    match error {
        DiagnosticError::Durable(_)
        | DiagnosticError::Store(_)
        | DiagnosticError::Plugin(_)
        | DiagnosticError::Prompt(_)
        | DiagnosticError::ActorKey(_) => {}
    }
}

fn main() {}
