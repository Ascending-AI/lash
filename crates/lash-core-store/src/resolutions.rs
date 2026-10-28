//! Execution-owned immutable deferred resolution outcomes.
//! Absent means unresolved; infrastructure failures create no terminal row.
use crate::tool_surface::OperationRoute;
use serde::{Deserialize, Serialize};
use {
    lash_kernel_value::names::*, lash_kernel_value::operations::*, lash_kernel_value::package::*,
    lash_vocabulary::ToolDefinition, lash_vocabulary::claims::*,
};

// resolutions: ExecutionId wraps the existing stable execution address encoding.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
/// Stable domain-qualified cell/process execution address, reused on retry, cold resume and adoption.
pub struct ExecutionId(pub String);
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// One deferred OperationUse in one execution, independent of activation or invocation identity.
pub struct ResolutionKey {
    pub execution: ExecutionId,
    pub use_: OperationUse,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
/// One immutable terminal resolution. Infrastructure failures create no row; accepted targets must be Waited.
pub enum ResolutionEntry {
    Resolved {
        definition: Box<ToolDefinition>,
        contract: ContractId,
        route: Box<OperationRoute>,
        mode: OperationMode,
        requirements: std::sync::Arc<ClaimRequirements>,
    },
    Refused {
        note: String,
    },
    Unavailable {
        note: Option<String>,
    },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// Requested name and committed outcome. Concurrent first callers use the winning row under distinct call identities.
pub struct ResolutionRecord {
    pub key: ResolutionKey,
    pub requested: QualifiedName,
    pub entry: ResolutionEntry,
}

impl ResolutionEntry {
    /// Target debt is retained until execution termination, independently of calls.
    pub fn requirements(&self) -> Option<&ClaimRequirements> {
        match self {
            Self::Resolved { requirements, .. } => Some(requirements),
            Self::Refused { .. } | Self::Unavailable { .. } => None,
        }
    }

    /// Reject contradictory pins before committing or consuming a stored outcome.
    pub fn validate(&self) -> Result<(), String> {
        let Self::Resolved {
            definition,
            contract,
            route,
            mode,
            requirements,
        } = self
        else {
            return Ok(());
        };
        if *mode != OperationMode::Waited
            || route.policy == lash_kernel_value::ExecutionPolicy::Read
        {
            return Err("deferred resolutions must be waited".into());
        }
        if definition.manifest.id != route.tool || definition.contract_id() != *contract {
            return Err("deferred definition, tool and contract disagree".into());
        }
        if !matches!(&route.source, crate::tool_surface::RouteSource::Resolved { definition: retained } if retained == definition)
        {
            return Err("deferred route must retain exactly the resolved definition".into());
        }
        if route.policy != definition.manifest.execution_policy
            || route.projection != definition.manifest.argument_projection
        {
            return Err("deferred route policy or projection disagrees with definition".into());
        }
        let mut target = ClaimRequirements::default();
        route.require(&mut target);
        let mut supplied = target.clone();
        supplied.extend(requirements);
        if supplied != **requirements {
            return Err("deferred resolution omits target claim requirements".into());
        }
        Ok(())
    }
}

impl ExecutionId {
    /// Stable process identity, independent of activation and Link identity.
    pub fn process(process: &lash_vocabulary::identity::ProcessId) -> Self {
        Self(format!("process:{process}"))
    }

    /// Cell occurrence is length-delimited so user-controlled addresses cannot alias.
    pub fn cell(session: &lash_kernel_value::identity::SessionId, turn: &str, cell: &str) -> Self {
        let session = session.to_string();
        Self(format!(
            "cell:{}:{session}{}:{turn}{}:{cell}",
            session.len(),
            turn.len(),
            cell.len()
        ))
    }
}

/// Explicit trusted spawn-policy pin, validated before a new execution becomes visible.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolutionSeed {
    pub use_: OperationUse,
    pub entry: ResolutionEntry,
}
