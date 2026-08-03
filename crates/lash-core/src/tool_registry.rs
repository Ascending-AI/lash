use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};

use crate::{
    PreparedToolCall, ProgressSender, ToolCall, ToolContext, ToolContract, ToolExecutionGrant,
    ToolId, ToolManifest, ToolPrepareCall, ToolProvider, ToolResult,
};

#[cfg(test)]
use self::tool_registry_facade_ops::ToolRegistryFacadeOps;
use self::tool_state_facade_ops::ToolStateFacadeOps;

include!("tool_registry/state.rs");
include!("tool_registry/sources.rs");
include!("tool_registry/registry_types.rs");
include!("tool_registry/registry_impl.rs");
include!("tool_registry/restore_execute.rs");
include!("tool_registry/rebind.rs");
include!("tool_registry/tests.rs");
