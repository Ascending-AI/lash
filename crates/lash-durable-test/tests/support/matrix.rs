//! FIG-5279: optional default/test-lease verdict equivalence. Normal law
//! runs pay only for the short lease; set LASH_MATRIX_VERIFY_LEASE=1 to
//! compare every cell and record both runs' timer steps and wall times.

use lash_durable_test::{Matrix, MatrixReport, Scenario};

pub trait MatrixTestExt {
    async fn run_test<S: Scenario>(&self, make: impl Fn() -> S) -> MatrixReport;
}

impl MatrixTestExt for Matrix {
    #[allow(
        clippy::disallowed_methods,
        reason = "the test host selects an optional proof from its action environment"
    )]
    async fn run_test<S: Scenario>(&self, make: impl Fn() -> S) -> MatrixReport {
        if std::env::var("LASH_MATRIX_VERIFY_LEASE").as_deref() == Ok("1") {
            self.run_lease_equivalence(make).await
        } else {
            self.run(make).await
        }
    }
}
