//! Laws of durable backend assembly (I0, FIG-5194; ADR 0132 §1): a backend
//! is refused, never defaulted, when its parts disagree.

use std::any::Any;
use std::sync::Arc;

use lash_core::{
    Backend, BackendParts, DurableBuildError, DurableSettings, NoProjectionProviders,
    ProjectionProviders,
};

fn parts() -> BackendParts {
    BackendParts {
        formats: Vec::new(),
        stores: crate::conformance::StoreLawBackend::stores(),
        settings: DurableSettings::default(),
        engines: Vec::new(),
        providers: Arc::new(NoProjectionProviders),
    }
}

fn refusal(parts: BackendParts) -> DurableBuildError {
    match Backend::assemble(parts) {
        Ok(_) => panic!("the backend assembled"),
        Err(error) => error,
    }
}

#[test]
fn two_engines_of_one_kind_are_refused() {
    let refused = refusal(BackendParts {
        engines: vec![
            Arc::new(lash_core::testing::FixtureProcessEngine),
            Arc::new(lash_core::testing::FixtureProcessEngine),
        ],
        ..parts()
    });
    assert!(
        matches!(&refused, DurableBuildError::DuplicateEngine { kind } if kind == "testing-fixture"),
        "{refused:?}"
    );
}

struct Projections(Vec<&'static str>);

impl ProjectionProviders for Projections {
    fn projection_types(&self) -> Vec<String> {
        self.0.iter().map(|kind| (*kind).to_owned()).collect()
    }

    fn as_any(&self) -> &(dyn Any + Send + Sync) {
        self
    }
}

#[test]
fn two_providers_of_one_projection_type_are_refused() {
    let refused = refusal(BackendParts {
        providers: Arc::new(Projections(vec!["page", "issue", "page"])),
        ..parts()
    });
    assert!(
        matches!(&refused, DurableBuildError::DuplicateProvider { projection } if projection == "page"),
        "{refused:?}"
    );
}

#[test]
fn settings_that_break_a_rule_are_refused() {
    let settings = DurableSettings {
        claim_batch: 0,
        ..DurableSettings::default()
    };
    let refused = refusal(BackendParts {
        settings,
        ..parts()
    });
    assert!(
        matches!(refused, DurableBuildError::InvalidConfig(_)),
        "{refused:?}"
    );
}
