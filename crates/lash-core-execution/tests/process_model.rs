//! Public process lease wire contracts, with their original unit-test names.

#![expect(
    clippy::expect_used,
    reason = "serialization fixture helpers assert that their setup is valid"
)]

use lash_core_execution::LeaseOwnerIdentity;

mod runtime {
    mod process {
        use lash_core_execution::runtime::{
            PROCESS_LEASE_SCHEMA_VERSION, ProcessId, ProcessLease, ProcessLeaseClaimOutcome,
            ProcessLeaseSchemaVersionError, ensure_process_lease_schema_version,
        };

        mod lease_serde_tests;
    }
}
