use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::artifact::ModuleArtifact;
use crate::ast::{AstString, Expr, TypeExpr, TypeField, format_type_expr};
use crate::identity::{ProcessDefinitionIdentity, ProcessDefinitionIdentityError};
use crate::linker::{LashlangHostCatalog, LashlangHostCatalogError, NamedDataType};
use crate::runtime::{LASH_HOST_DESCRIPTOR_TYPE_KEY, LASH_HOST_DESCRIPTOR_VALUE_KEY};

const TRIGGERS_RESOURCE_TYPE: &str = "Triggers";
const TRIGGERS_ALIAS: &str = "triggers";
const TRIGGER_REGISTRATION_TYPE: &str = "lash.TriggerRegistration";
pub const LASH_TRIGGER_EVENT_KEY: &str = "$lash.trigger.event";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TriggerHostOperation {
    Register,
    List,
    Update,
    Enable,
    Disable,
    Delete,
    Revive,
    Prune,
}

impl TriggerHostOperation {
    pub const fn host_operation(self) -> &'static str {
        match self {
            Self::Register => "triggers.register",
            Self::List => "triggers.list",
            Self::Update => "triggers.update",
            Self::Enable => "triggers.enable",
            Self::Disable => "triggers.disable",
            Self::Delete => "triggers.delete",
            Self::Revive => "triggers.revive",
            Self::Prune => "triggers.prune",
        }
    }

    pub const fn receiver_method(self) -> &'static str {
        match self {
            Self::Register => "register",
            Self::List => "list",
            Self::Update => "update",
            Self::Enable => "enable",
            Self::Disable => "disable",
            Self::Delete => "delete",
            Self::Revive => "revive",
            Self::Prune => "prune",
        }
    }

    pub fn from_host_operation(operation: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|candidate| candidate.host_operation() == operation)
    }

    pub fn from_receiver_method(operation: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|candidate| candidate.receiver_method() == operation)
    }

    pub fn input_ty(self) -> TypeExpr {
        match self {
            Self::Register => TypeExpr::Object(vec![
                required_field("source", TypeExpr::Dict),
                required_field(
                    "target",
                    TypeExpr::Process {
                        input: Box::new(TypeExpr::Any),
                        output: Box::new(TypeExpr::Any),
                        input_count: 1,
                    },
                ),
                required_field("inputs", TypeExpr::Dict),
                optional_field("name", TypeExpr::Str),
                optional_field("subscription_key", TypeExpr::Str),
            ]),
            Self::List => TypeExpr::Object(vec![
                optional_field(
                    "target",
                    TypeExpr::Process {
                        input: Box::new(TypeExpr::Any),
                        output: Box::new(TypeExpr::Any),
                        input_count: 1,
                    },
                ),
                optional_field("name", TypeExpr::Str),
                optional_field("source_type", TypeExpr::Str),
                optional_field("enabled", TypeExpr::Bool),
            ]),
            Self::Update | Self::Revive => TypeExpr::Object(vec![
                required_field("subscription_key", TypeExpr::Str),
                required_field("expected_revision", TypeExpr::Int),
                required_field("source", TypeExpr::Dict),
                required_field(
                    "target",
                    TypeExpr::Process {
                        input: Box::new(TypeExpr::Any),
                        output: Box::new(TypeExpr::Any),
                        input_count: 1,
                    },
                ),
                required_field("inputs", TypeExpr::Dict),
                optional_field("name", TypeExpr::Str),
            ]),
            Self::Enable | Self::Disable | Self::Delete => TypeExpr::Object(vec![
                required_field("subscription_key", TypeExpr::Str),
                required_field("expected_revision", TypeExpr::Int),
            ]),
            Self::Prune => TypeExpr::Object(vec![required_field(
                "subscription_keys",
                TypeExpr::List(Box::new(TypeExpr::Str)),
            )]),
        }
    }

    pub fn output_ty(self) -> TypeExpr {
        match self {
            Self::Register => TypeExpr::TriggerHandle(Box::new(TypeExpr::Any)),
            Self::List => TypeExpr::List(Box::new(TypeExpr::Ref(TRIGGER_REGISTRATION_TYPE.into()))),
            Self::Update | Self::Enable | Self::Disable | Self::Delete | Self::Revive => {
                TypeExpr::TriggerHandle(Box::new(TypeExpr::Any))
            }
            Self::Prune => {
                TypeExpr::List(Box::new(TypeExpr::TriggerHandle(Box::new(TypeExpr::Any))))
            }
        }
    }

    pub const ALL: [Self; 8] = [
        Self::Register,
        Self::List,
        Self::Update,
        Self::Enable,
        Self::Disable,
        Self::Delete,
        Self::Revive,
        Self::Prune,
    ];
}

pub fn is_trigger_resource_type(resource_type: &str) -> bool {
    resource_type == TRIGGERS_RESOURCE_TYPE
}

pub fn add_trigger_resource_operations(
    catalog: &mut LashlangHostCatalog,
) -> Result<(), LashlangHostCatalogError> {
    let mut extended = catalog.clone();
    extended.add_named_data_type(
        NamedDataType::object(
            TRIGGER_REGISTRATION_TYPE,
            vec![
                required_field("subscription_key", TypeExpr::Str),
                required_field("incarnation", TypeExpr::Str),
                required_field("revision", TypeExpr::Int),
                required_field("registrant", TypeExpr::Dict),
                required_field(
                    "manifest_membership",
                    TypeExpr::Enum(vec![
                        "present_in_current_artifact".into(),
                        "orphaned".into(),
                        "unknown".into(),
                    ]),
                ),
                required_field("source_key", TypeExpr::Str),
                optional_field("name", TypeExpr::Str),
                required_field("source_type", TypeExpr::Str),
                required_field("source", TypeExpr::Dict),
                required_field("target", TypeExpr::Dict),
                required_field("enabled", TypeExpr::Bool),
            ],
        )
        .expect("trigger registration is a valid named data type"),
    )?;
    for operation in TriggerHostOperation::ALL {
        extended.add_module_operation(
            [TRIGGERS_ALIAS],
            TRIGGERS_RESOURCE_TYPE,
            operation.receiver_method(),
            operation.host_operation(),
            operation.input_ty(),
            operation.output_ty(),
        )?;
    }
    *catalog = extended;
    Ok(())
}

fn required_field(name: &'static str, ty: TypeExpr) -> TypeField {
    TypeField {
        name: name.into(),
        ty,
        optional: false,
    }
}

fn optional_field(name: &'static str, ty: TypeExpr) -> TypeField {
    TypeField {
        name: name.into(),
        ty,
        optional: true,
    }
}

pub struct TriggerRegistrationCall<'expr> {
    pub source: &'expr Expr,
    pub target: &'expr Expr,
    pub inputs: &'expr Expr,
    pub name: Option<&'expr Expr>,
    pub subscription_key: Option<&'expr Expr>,
}

pub struct TriggerListCall<'expr> {
    pub entries: &'expr [(AstString, Expr)],
}

pub fn register_call_args(
    args: &[Expr],
) -> Result<TriggerRegistrationCall<'_>, TriggerCallShapeError> {
    let entries = record_entries(args).ok_or(TriggerCallShapeError::Registration)?;
    Ok(TriggerRegistrationCall {
        source: required_entry(entries, "source").ok_or(TriggerCallShapeError::Registration)?,
        target: required_entry(entries, "target").ok_or(TriggerCallShapeError::Registration)?,
        inputs: required_entry(entries, "inputs").ok_or(TriggerCallShapeError::Registration)?,
        name: required_entry(entries, "name"),
        subscription_key: required_entry(entries, "subscription_key"),
    })
}

pub fn list_call_args(args: &[Expr]) -> Result<TriggerListCall<'_>, TriggerCallShapeError> {
    let entries = record_entries(args).ok_or(TriggerCallShapeError::List)?;
    for (name, _) in entries {
        match name.as_str() {
            "target" | "name" | "source_type" | "enabled" => {}
            _ => return Err(TriggerCallShapeError::List),
        }
    }
    Ok(TriggerListCall { entries })
}

fn record_entries(args: &[Expr]) -> Option<&[(AstString, Expr)]> {
    let [Expr::Record(entries)] = args else {
        return None;
    };
    Some(entries)
}

fn required_entry<'expr>(entries: &'expr [(AstString, Expr)], name: &str) -> Option<&'expr Expr> {
    entries
        .iter()
        .find_map(|(entry_name, expr)| (entry_name.as_str() == name).then_some(expr))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TriggerCallShapeError {
    Registration,
    List,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TriggerRegistrationRequest {
    pub source: HostDescriptor,
    pub target: ProcessDefinitionIdentity,
    pub inputs: TriggerInputTemplate,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subscription_key: Option<String>,
}

impl TriggerRegistrationRequest {
    pub fn decode(request: &serde_json::Value) -> Result<Self, TriggerRequestDecodeError> {
        let operation = TriggerHostOperation::Register;
        Ok(Self {
            source: HostDescriptor::decode(required_json_field(request, "source", operation)?)
                .map_err(TriggerRequestDecodeError::from)?,
            target: decode_process_definition_identity(
                required_json_field(request, "target", operation)?,
                "trigger target",
            )?,
            inputs: TriggerInputTemplate::decode(required_json_field(
                request, "inputs", operation,
            )?)?,
            name: request
                .get("name")
                .and_then(serde_json::Value::as_str)
                .map(ToOwned::to_owned),
            subscription_key: request
                .get("subscription_key")
                .map(|value| {
                    value.as_str().map(ToOwned::to_owned).ok_or_else(|| {
                        TriggerRequestDecodeError::InvalidField {
                            operation: operation.host_operation(),
                            field: "subscription_key",
                            message: "expected a string literal".to_string(),
                        }
                    })
                })
                .transpose()?,
        })
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TriggerInputTemplate {
    entries: BTreeMap<String, TriggerInputBinding>,
}

impl TriggerInputTemplate {
    pub fn new(entries: BTreeMap<String, TriggerInputBinding>) -> Self {
        Self { entries }
    }

    pub fn decode(value: &serde_json::Value) -> Result<Self, TriggerRequestDecodeError> {
        let map = value
            .as_object()
            .ok_or_else(|| TriggerRequestDecodeError::InvalidField {
                operation: TriggerHostOperation::Register.host_operation(),
                field: "inputs",
                message: "expected an object mapping process params to values".to_string(),
            })?;
        let mut entries = BTreeMap::new();
        for (name, value) in map {
            let binding = if is_trigger_event_placeholder_value(value) {
                TriggerInputBinding::Event
            } else {
                TriggerInputBinding::Fixed {
                    value: value.clone(),
                }
            };
            entries.insert(name.clone(), binding);
        }
        Ok(Self { entries })
    }

    pub fn entries(&self) -> impl Iterator<Item = (&str, &TriggerInputBinding)> {
        self.entries
            .iter()
            .map(|(name, binding)| (name.as_str(), binding))
    }

    pub fn get(&self, name: &str) -> Option<&TriggerInputBinding> {
        self.entries.get(name)
    }

    pub fn contains_event(&self) -> bool {
        self.entries
            .values()
            .any(|binding| matches!(binding, TriggerInputBinding::Event))
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TriggerInputBinding {
    Event,
    Fixed { value: serde_json::Value },
}

impl TriggerInputBinding {
    pub fn as_fixed(&self) -> Option<&serde_json::Value> {
        match self {
            Self::Fixed { value } => Some(value),
            Self::Event => None,
        }
    }
}

pub fn trigger_event_placeholder_expr() -> Expr {
    Expr::Record(vec![(LASH_TRIGGER_EVENT_KEY.into(), Expr::Bool(true))])
}

fn is_trigger_event_placeholder_value(value: &serde_json::Value) -> bool {
    let Some(map) = value.as_object() else {
        return false;
    };
    map.len() == 1
        && map
            .get(LASH_TRIGGER_EVENT_KEY)
            .and_then(serde_json::Value::as_bool)
            == Some(true)
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TriggerListRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<ProcessDefinitionIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}

impl TriggerListRequest {
    pub fn decode(request: &serde_json::Value) -> Result<Self, TriggerRequestDecodeError> {
        let map = request
            .as_object()
            .ok_or_else(|| TriggerRequestDecodeError::InvalidField {
                operation: TriggerHostOperation::List.host_operation(),
                field: "filters",
                message: "expected a record of trigger filters".to_string(),
            })?;
        for key in map.keys() {
            match key.as_str() {
                "target" | "name" | "source_type" | "enabled" => {}
                _ => {
                    return Err(TriggerRequestDecodeError::InvalidField {
                        operation: TriggerHostOperation::List.host_operation(),
                        field: "filters",
                        message: format!("unknown filter `{key}`"),
                    });
                }
            }
        }
        Ok(Self {
            target: request
                .get("target")
                .map(|value| decode_process_definition_identity(value, "triggers.list target"))
                .transpose()?,
            name: optional_string_filter(request, "name", TriggerHostOperation::List)?,
            source_type: optional_string_filter(
                request,
                "source_type",
                TriggerHostOperation::List,
            )?,
            enabled: optional_bool_filter(request, "enabled", TriggerHostOperation::List)?,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TriggerPruneRequest {
    pub subscription_keys: Vec<String>,
}

impl TriggerPruneRequest {
    pub fn decode(request: &serde_json::Value) -> Result<Self, TriggerRequestDecodeError> {
        let value = required_json_field(request, "subscription_keys", TriggerHostOperation::Prune)?;
        let values = value
            .as_array()
            .ok_or_else(|| TriggerRequestDecodeError::InvalidField {
                operation: TriggerHostOperation::Prune.host_operation(),
                field: "subscription_keys",
                message: "expected a list of subscription key strings".to_string(),
            })?;
        let mut subscription_keys = BTreeSet::new();
        for value in values {
            let key = value
                .as_str()
                .ok_or_else(|| TriggerRequestDecodeError::InvalidField {
                    operation: TriggerHostOperation::Prune.host_operation(),
                    field: "subscription_keys",
                    message: "expected a list of subscription key strings".to_string(),
                })?;
            if key.trim().is_empty() {
                return Err(TriggerRequestDecodeError::InvalidField {
                    operation: TriggerHostOperation::Prune.host_operation(),
                    field: "subscription_keys",
                    message: "subscription keys must be non-empty".to_string(),
                });
            }
            subscription_keys.insert(key.to_string());
        }
        Ok(Self {
            subscription_keys: subscription_keys.into_iter().collect(),
        })
    }
}

fn optional_string_filter(
    request: &serde_json::Value,
    field: &'static str,
    operation: TriggerHostOperation,
) -> Result<Option<String>, TriggerRequestDecodeError> {
    request
        .get(field)
        .map(|value| {
            value.as_str().map(ToOwned::to_owned).ok_or_else(|| {
                TriggerRequestDecodeError::InvalidField {
                    operation: operation.host_operation(),
                    field,
                    message: "expected a string".to_string(),
                }
            })
        })
        .transpose()
}

fn optional_bool_filter(
    request: &serde_json::Value,
    field: &'static str,
    operation: TriggerHostOperation,
) -> Result<Option<bool>, TriggerRequestDecodeError> {
    request
        .get(field)
        .map(|value| {
            value
                .as_bool()
                .ok_or_else(|| TriggerRequestDecodeError::InvalidField {
                    operation: operation.host_operation(),
                    field,
                    message: "expected a boolean".to_string(),
                })
        })
        .transpose()
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HostDescriptor {
    pub source_type: String,
    pub value: serde_json::Value,
}

impl HostDescriptor {
    pub fn new(source_type: impl Into<String>, value: serde_json::Value) -> Self {
        Self {
            source_type: source_type.into(),
            value,
        }
    }

    pub fn decode(source: &serde_json::Value) -> Result<Self, HostDescriptorError> {
        let source_type = source
            .get(LASH_HOST_DESCRIPTOR_TYPE_KEY)
            .and_then(serde_json::Value::as_str)
            .map(ToOwned::to_owned)
            .ok_or(HostDescriptorError::InvalidHostDescriptor)?;
        let value = source
            .get(LASH_HOST_DESCRIPTOR_VALUE_KEY)
            .cloned()
            .ok_or(HostDescriptorError::InvalidHostDescriptor)?;
        Ok(Self { source_type, value })
    }

    pub fn encode(
        source_type: impl Into<String>,
        value: impl Serialize,
    ) -> Result<serde_json::Value, HostDescriptorError> {
        let source_type = source_type.into();
        let value =
            serde_json::to_value(value).map_err(|err| HostDescriptorError::MalformedPayload {
                source_type: source_type.clone(),
                message: err.to_string(),
            })?;
        Ok(Self::new(source_type, value).to_json())
    }

    pub fn decode_as<T: serde::de::DeserializeOwned>(
        &self,
        resources: &LashlangHostCatalog,
    ) -> Result<T, HostDescriptorError> {
        resources.decode_host_descriptor_as(&self.source_type, self.value.clone())
    }

    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            LASH_HOST_DESCRIPTOR_TYPE_KEY: self.source_type,
            LASH_HOST_DESCRIPTOR_VALUE_KEY: self.value,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum HostDescriptorError {
    #[error("host descriptor must be a host descriptor constructor result")]
    InvalidHostDescriptor,
    #[error("host descriptor `{source_type}` is not declared in the host catalog")]
    UnknownSourceType { source_type: String },
    #[error("host descriptor `{source_type}` payload is invalid: {message}")]
    MalformedPayload {
        source_type: String,
        message: String,
    },
}

pub fn event_type_for_source(
    resources: &LashlangHostCatalog,
    source_type: &str,
) -> Result<NamedDataType, TriggerRequestDecodeError> {
    resources
        .resolve_trigger_source(source_type)
        .map(|binding| binding.event_type().clone())
        .ok_or_else(|| TriggerRequestDecodeError::UnknownSourceType {
            source_type: source_type.to_string(),
        })
}

#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum TriggerRequestDecodeError {
    #[error("{operation} requires `{field}`")]
    MissingField {
        operation: &'static str,
        field: &'static str,
    },
    #[error("{operation} field `{field}` is invalid: {message}")]
    InvalidField {
        operation: &'static str,
        field: &'static str,
        message: String,
    },
    #[error("trigger source must be a host descriptor constructor result")]
    InvalidSource,
    #[error("{label} {message}")]
    InvalidTarget {
        label: &'static str,
        message: String,
    },
    #[error("host descriptor `{source_type}` is not registered as a trigger source")]
    UnknownSourceType { source_type: String },
}

impl From<HostDescriptorError> for TriggerRequestDecodeError {
    fn from(err: HostDescriptorError) -> Self {
        match err {
            HostDescriptorError::InvalidHostDescriptor => Self::InvalidSource,
            HostDescriptorError::UnknownSourceType { source_type } => {
                Self::UnknownSourceType { source_type }
            }
            HostDescriptorError::MalformedPayload { message, .. } => Self::InvalidField {
                operation: TriggerHostOperation::Register.host_operation(),
                field: "source",
                message,
            },
        }
    }
}

fn required_json_field<'json>(
    request: &'json serde_json::Value,
    field: &'static str,
    operation: TriggerHostOperation,
) -> Result<&'json serde_json::Value, TriggerRequestDecodeError> {
    request
        .get(field)
        .ok_or_else(|| TriggerRequestDecodeError::MissingField {
            operation: operation.host_operation(),
            field,
        })
}

fn decode_process_definition_identity(
    value: &serde_json::Value,
    label: &'static str,
) -> Result<ProcessDefinitionIdentity, TriggerRequestDecodeError> {
    ProcessDefinitionIdentity::from_process_value(value).map_err(|err| {
        TriggerRequestDecodeError::InvalidTarget {
            label,
            message: match err {
                ProcessDefinitionIdentityError::NotProcessValue => {
                    "must be a process value".to_string()
                }
                ProcessDefinitionIdentityError::MissingField { field } => {
                    format!("missing {field}")
                }
                ProcessDefinitionIdentityError::InvalidField { field, message } => {
                    format!("invalid {field}: {message}")
                }
            },
        }
    })
}

#[derive(Clone, Debug, PartialEq)]
struct TriggerCompatibilityValidation {
    inputs: TriggerInputTemplate,
    event_ty: TypeExpr,
}

pub struct TriggerCompatibilityRequest<'a> {
    pub artifact: &'a ModuleArtifact,
    pub definition: &'a ProcessDefinitionIdentity,
    pub source_type: &'a str,
    pub inputs: &'a TriggerInputTemplate,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TriggerCompatibility {
    pub definition: ProcessDefinitionIdentity,
    pub event_type: NamedDataType,
    pub resolved_event_type: TypeExpr,
    pub inputs: TriggerInputTemplate,
}

pub fn check_trigger_compatibility(
    request: TriggerCompatibilityRequest<'_>,
) -> Result<TriggerCompatibility, TriggerCompatibilityError> {
    let event_type = event_type_for_source(
        &request.artifact.host_requirements.resources,
        request.source_type,
    )
    .map_err(|err| match err {
        TriggerRequestDecodeError::UnknownSourceType { source_type } => {
            TriggerCompatibilityError::UnknownSourceType { source_type }
        }
        other => TriggerCompatibilityError::Source(other.to_string()),
    })?;
    let validation = validate_trigger_compatibility_target(
        request.definition,
        &event_type,
        request.inputs,
        request.artifact,
    )?;
    Ok(TriggerCompatibility {
        definition: request.definition.clone(),
        event_type,
        resolved_event_type: validation.event_ty,
        inputs: validation.inputs,
    })
}

#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum TriggerCompatibilityError {
    #[error("host descriptor `{source_type}` is not registered as a trigger source")]
    UnknownSourceType { source_type: String },
    #[error("trigger source is invalid: {0}")]
    Source(String),
    #[error(
        "trigger target `{process_name}` module ref mismatch: target has {target_module_ref}, artifact has {artifact_module_ref}"
    )]
    ModuleRefMismatch {
        process_name: String,
        target_module_ref: String,
        artifact_module_ref: String,
    },
    #[error(
        "trigger target `{process_name}` host requirements mismatch: target has {target_host_requirements}, artifact has {artifact_host_requirements}"
    )]
    HostRequirementsMismatch {
        process_name: String,
        target_host_requirements: String,
        artifact_host_requirements: String,
    },
    #[error(
        "trigger target artifact `{module_ref}` does not export process `{process_name}` as requested ref {process_ref}"
    )]
    ProcessRefMismatch {
        module_ref: String,
        process_name: String,
        process_ref: String,
    },
    #[error("trigger target artifact `{module_ref}` is missing process `{process_name}`")]
    MissingProcess {
        module_ref: String,
        process_name: String,
    },
    #[error("trigger target `{process_name}` input `{input}` is not mapped")]
    MissingInput { process_name: String, input: String },
    #[error("trigger target `{process_name}` has no input `{input}`")]
    UnknownInput { process_name: String, input: String },
    #[error("trigger target `{process_name}` inputs must map at least one param to trigger.event")]
    MissingEventInput { process_name: String },
    #[error(
        "trigger source emits {event}, but target `{process_name}` input `{input_name}` expects {input}"
    )]
    EventMismatch {
        event: String,
        process_name: String,
        input_name: String,
        input: String,
    },
    #[error(
        "trigger target `{process_name}` input `{input}` has incompatible fixed authority type: expected {expected}, got {actual}"
    )]
    FixedInputMismatch {
        process_name: String,
        input: String,
        expected: String,
        actual: String,
    },
}

fn validate_trigger_compatibility_target(
    target: &ProcessDefinitionIdentity,
    event_ty: &NamedDataType,
    inputs: &TriggerInputTemplate,
    artifact: &ModuleArtifact,
) -> Result<TriggerCompatibilityValidation, TriggerCompatibilityError> {
    if artifact.module_ref != target.module_ref {
        return Err(TriggerCompatibilityError::ModuleRefMismatch {
            process_name: target.process_name.clone(),
            target_module_ref: target.module_ref.to_string(),
            artifact_module_ref: artifact.module_ref.to_string(),
        });
    }
    if artifact.host_requirements_ref != target.host_requirements_ref {
        return Err(TriggerCompatibilityError::HostRequirementsMismatch {
            process_name: target.process_name.clone(),
            target_host_requirements: target.host_requirements_ref.to_string(),
            artifact_host_requirements: artifact.host_requirements_ref.to_string(),
        });
    }
    let Some(exported_process_name) = artifact.process_name_for_ref(&target.process_ref) else {
        return Err(TriggerCompatibilityError::ProcessRefMismatch {
            module_ref: target.module_ref.to_string(),
            process_name: target.process_name.clone(),
            process_ref: format!("{:?}", target.process_ref),
        });
    };
    if exported_process_name != target.process_name {
        return Err(TriggerCompatibilityError::ProcessRefMismatch {
            module_ref: target.module_ref.to_string(),
            process_name: target.process_name.clone(),
            process_ref: format!("{:?}", target.process_ref),
        });
    }
    let process = artifact
        .canonical_ir
        .process(exported_process_name)
        .ok_or_else(|| TriggerCompatibilityError::MissingProcess {
            module_ref: target.module_ref.to_string(),
            process_name: target.process_name.clone(),
        })?;
    for (input_name, _) in inputs.entries() {
        if !process
            .params
            .iter()
            .any(|param| param.name.as_str() == input_name)
        {
            return Err(TriggerCompatibilityError::UnknownInput {
                process_name: target.process_name.clone(),
                input: input_name.to_string(),
            });
        }
    }
    if !inputs.contains_event() {
        return Err(TriggerCompatibilityError::MissingEventInput {
            process_name: target.process_name.clone(),
        });
    }
    let aliases = type_aliases(artifact);
    let event_ty = resolve_type_refs(
        &event_ty.to_ref_ty(),
        &aliases,
        &artifact.host_requirements.resources,
    );
    for param in &process.params {
        let Some(input) = inputs.get(param.name.as_str()) else {
            return Err(TriggerCompatibilityError::MissingInput {
                process_name: target.process_name.clone(),
                input: param.name.to_string(),
            });
        };
        let input_ty =
            resolve_type_refs(&param.ty, &aliases, &artifact.host_requirements.resources);
        match input {
            TriggerInputBinding::Event => {
                if !is_resolved_type_assignable(&event_ty, &input_ty) {
                    return Err(TriggerCompatibilityError::EventMismatch {
                        event: format_type_expr(&event_ty),
                        process_name: target.process_name.clone(),
                        input_name: param.name.to_string(),
                        input: format_type_expr(&input_ty),
                    });
                }
            }
            TriggerInputBinding::Fixed { value } => {
                validate_fixed_input_value(
                    value,
                    &input_ty,
                    &artifact.host_requirements.resources,
                    target.process_name.as_str(),
                    param.name.as_str(),
                )?;
            }
        }
    }
    Ok(TriggerCompatibilityValidation {
        inputs: inputs.clone(),
        event_ty,
    })
}

fn validate_fixed_input_value(
    value: &serde_json::Value,
    input_ty: &TypeExpr,
    resources: &LashlangHostCatalog,
    process_name: &str,
    input_name: &str,
) -> Result<(), TriggerCompatibilityError> {
    let TypeExpr::Ref(resource_type) = input_ty else {
        return Ok(());
    };
    if !resources.has_resource_type(resource_type.as_str()) {
        return Ok(());
    }
    match crate::runtime::from_json(value.clone()) {
        crate::Value::Resource(handle) if handle.resource_type == *resource_type => Ok(()),
        crate::Value::Resource(handle) => Err(TriggerCompatibilityError::FixedInputMismatch {
            process_name: process_name.to_string(),
            input: input_name.to_string(),
            expected: resource_type.to_string(),
            actual: handle.resource_type,
        }),
        _ => Err(TriggerCompatibilityError::FixedInputMismatch {
            process_name: process_name.to_string(),
            input: input_name.to_string(),
            expected: resource_type.to_string(),
            actual: "value".to_string(),
        }),
    }
}

fn resolve_type_refs(
    ty: &TypeExpr,
    aliases: &BTreeMap<String, TypeExpr>,
    resources: &LashlangHostCatalog,
) -> TypeExpr {
    resolve_type_refs_inner(ty, aliases, Some(resources), &mut BTreeSet::new())
}

fn resolve_type_refs_inner(
    ty: &TypeExpr,
    aliases: &BTreeMap<String, TypeExpr>,
    resources: Option<&LashlangHostCatalog>,
    seen: &mut BTreeSet<String>,
) -> TypeExpr {
    match ty {
        TypeExpr::Ref(name) if seen.insert(name.to_string()) => {
            let resolved = if let Some(ty) = aliases.get(name.as_str()) {
                resolve_type_refs_inner(ty, aliases, resources, seen)
            } else if let Some(data_type) =
                resources.and_then(|resources| resources.resolve_named_data_type(name.as_str()))
            {
                data_type.ty().clone()
            } else {
                ty.clone()
            };
            seen.remove(name.as_str());
            resolved
        }
        TypeExpr::List(item) => TypeExpr::List(Box::new(resolve_type_refs_inner(
            item, aliases, resources, seen,
        ))),
        TypeExpr::Object(fields) => TypeExpr::Object(
            fields
                .iter()
                .map(|field| TypeField {
                    name: field.name.clone(),
                    ty: resolve_type_refs_inner(&field.ty, aliases, resources, seen),
                    optional: field.optional,
                })
                .collect(),
        ),
        TypeExpr::Union(items) => TypeExpr::Union(
            items
                .iter()
                .map(|item| resolve_type_refs_inner(item, aliases, resources, seen))
                .collect(),
        ),
        TypeExpr::Process {
            input,
            output,
            input_count,
        } => TypeExpr::Process {
            input: Box::new(resolve_type_refs_inner(input, aliases, resources, seen)),
            output: Box::new(resolve_type_refs_inner(output, aliases, resources, seen)),
            input_count: *input_count,
        },
        TypeExpr::TriggerHandle(event) => TypeExpr::TriggerHandle(Box::new(
            resolve_type_refs_inner(event, aliases, resources, seen),
        )),
        _ => ty.clone(),
    }
}

pub fn is_resolved_type_assignable(source: &TypeExpr, target: &TypeExpr) -> bool {
    if matches!(source, TypeExpr::Any) || matches!(target, TypeExpr::Any) {
        return true;
    }
    if source == target {
        return true;
    }

    match (source, target) {
        (TypeExpr::Union(sources), _) => sources
            .iter()
            .all(|source| is_resolved_type_assignable(source, target)),
        (_, TypeExpr::Union(targets)) => targets
            .iter()
            .any(|target| is_resolved_type_assignable(source, target)),
        (TypeExpr::Int, TypeExpr::Float) => true,
        (TypeExpr::Str, TypeExpr::Enum(_)) => true,
        (TypeExpr::Enum(_), TypeExpr::Str) => true,
        (TypeExpr::Enum(sources), TypeExpr::Enum(targets)) => {
            sources.iter().all(|source| targets.contains(source))
        }
        (TypeExpr::List(source), TypeExpr::List(target)) => {
            is_resolved_type_assignable(source, target)
        }
        (TypeExpr::Dict, TypeExpr::Object(_)) => true,
        (TypeExpr::Object(_), TypeExpr::Dict) => true,
        (TypeExpr::Object(source), TypeExpr::Object(target)) => {
            object_type_assignable(source, target)
        }
        (TypeExpr::Ref(source), TypeExpr::Ref(target)) => source == target,
        (
            TypeExpr::Process {
                input: source_input,
                output: source_output,
                input_count: source_count,
            },
            TypeExpr::Process {
                input: target_input,
                output: target_output,
                input_count: target_count,
            },
        ) => {
            source_count == target_count
                && is_resolved_type_assignable(source_input, target_input)
                && is_resolved_type_assignable(source_output, target_output)
        }
        (TypeExpr::TriggerHandle(source), TypeExpr::TriggerHandle(target)) => {
            is_resolved_type_assignable(source, target)
        }
        _ => false,
    }
}

fn object_type_assignable(source: &[TypeField], target: &[TypeField]) -> bool {
    target.iter().all(|target_field| {
        let Some(source_field) = source
            .iter()
            .find(|source_field| source_field.name == target_field.name)
        else {
            return target_field.optional;
        };
        if !target_field.optional && source_field.optional {
            return false;
        }
        is_resolved_type_assignable(&source_field.ty, &target_field.ty)
    })
}

fn type_aliases(artifact: &ModuleArtifact) -> BTreeMap<String, TypeExpr> {
    artifact
        .canonical_ir
        .declarations
        .iter()
        .filter_map(|declaration| match declaration {
            crate::Declaration::Type(decl) => Some((decl.name.to_string(), decl.ty.clone())),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Deserialize, PartialEq)]
    struct ScheduleSource {
        expr: String,
        #[serde(default)]
        tz: Option<String>,
    }

    fn resources() -> LashlangHostCatalog {
        let mut resources = LashlangHostCatalog::new();
        resources
            .add_trigger_source_constructor(
                ["cron", "Schedule"],
                TypeExpr::Object(vec![
                    TypeField {
                        name: "expr".into(),
                        ty: TypeExpr::Str,
                        optional: false,
                    },
                    TypeField {
                        name: "tz".into(),
                        ty: TypeExpr::Str,
                        optional: true,
                    },
                ]),
                NamedDataType::object(
                    "cron.Tick",
                    vec![TypeField {
                        name: "fired_at".into(),
                        ty: TypeExpr::Str,
                        optional: false,
                    }],
                )
                .expect("valid cron tick type"),
            )
            .expect("valid cron schedule source");
        resources
    }

    fn process_environment(resources: LashlangHostCatalog) -> crate::LashlangHostEnvironment {
        crate::LashlangHostEnvironment::new(
            resources,
            crate::LashlangAbilities::default().with_processes(),
        )
    }

    fn linked_artifact(source: &str, resources: LashlangHostCatalog) -> ModuleArtifact {
        let source =
            format!("{source}\nsource = cron.Schedule({{ expr: \"*\" }})\nfinish source\n");
        crate::LinkedModule::link(
            crate::parse(&source).expect("parse trigger target module"),
            process_environment(resources),
        )
        .expect("link trigger target module")
        .artifact
    }

    fn event_input_template() -> TriggerInputTemplate {
        TriggerInputTemplate::new(BTreeMap::from([(
            "event".to_string(),
            TriggerInputBinding::Event,
        )]))
    }

    fn required_field(name: &str, ty: TypeExpr) -> TypeField {
        TypeField {
            name: name.into(),
            ty,
            optional: false,
        }
    }

    fn definition_for(artifact: &ModuleArtifact, process_name: &str) -> ProcessDefinitionIdentity {
        ProcessDefinitionIdentity::from_artifact_export(artifact, process_name)
            .expect("artifact should export process")
    }

    #[test]
    fn resolved_type_assignability_accepts_any_as_source_or_target() {
        assert!(is_resolved_type_assignable(&TypeExpr::Any, &TypeExpr::Str));
        assert!(is_resolved_type_assignable(&TypeExpr::Str, &TypeExpr::Any));
    }

    #[test]
    fn resolved_type_assignability_rejects_known_scalar_mismatches() {
        assert!(!is_resolved_type_assignable(&TypeExpr::Str, &TypeExpr::Int));
        assert!(!is_resolved_type_assignable(
            &TypeExpr::Bool,
            &TypeExpr::Float
        ));
    }

    #[test]
    fn resolved_type_assignability_treats_strings_as_consistent_with_string_enums() {
        let target = TypeExpr::Enum(vec!["a".into(), "b".into()]);

        assert!(is_resolved_type_assignable(&TypeExpr::Str, &target));
        assert!(!is_resolved_type_assignable(&TypeExpr::Int, &target));
        assert!(is_resolved_type_assignable(
            &TypeExpr::Enum(vec!["a".into()]),
            &TypeExpr::Str
        ));
    }

    #[test]
    fn resolved_type_assignability_checks_known_object_fields() {
        let target = TypeExpr::Object(vec![required_field("value", TypeExpr::Str)]);
        let compatible = TypeExpr::Object(vec![
            required_field("value", TypeExpr::Str),
            required_field("extra", TypeExpr::Int),
        ]);
        let incompatible = TypeExpr::Object(vec![required_field("value", TypeExpr::Int)]);

        assert!(is_resolved_type_assignable(&compatible, &target));
        assert!(!is_resolved_type_assignable(&incompatible, &target));
    }

    #[test]
    fn resolved_type_assignability_treats_dict_as_gradual_at_object_targets() {
        let object = TypeExpr::Object(vec![required_field("value", TypeExpr::Str)]);

        assert!(is_resolved_type_assignable(&TypeExpr::Dict, &object));
        assert!(is_resolved_type_assignable(&object, &TypeExpr::Dict));
        assert!(!is_resolved_type_assignable(&TypeExpr::Int, &object));
    }

    #[test]
    fn resolved_type_assignability_rejects_named_nested_list_mismatches() {
        assert!(!is_resolved_type_assignable(
            &TypeExpr::List(Box::new(TypeExpr::Str)),
            &TypeExpr::List(Box::new(TypeExpr::Int)),
        ));
    }

    #[test]
    fn resolved_type_assignability_accepts_named_nested_list_any_consistency() {
        assert!(is_resolved_type_assignable(
            &TypeExpr::List(Box::new(TypeExpr::Any)),
            &TypeExpr::List(Box::new(TypeExpr::Int)),
        ));
    }

    #[test]
    fn host_descriptor_encode_decode_and_typed_decode_round_trip() {
        let value = serde_json::json!({
            "expr": "*/10 * * * * *",
            "tz": "UTC",
        });
        let encoded =
            HostDescriptor::encode("cron.Schedule", value).expect("host descriptor encode");
        let decoded = HostDescriptor::decode(&encoded).expect("host descriptor decode");
        let payload: ScheduleSource = decoded
            .decode_as(&resources())
            .expect("typed host descriptor payload");

        assert_eq!(
            payload,
            ScheduleSource {
                expr: "*/10 * * * * *".to_string(),
                tz: Some("UTC".to_string()),
            }
        );
    }

    #[test]
    fn host_descriptor_typed_decode_rejects_unknown_source_type() {
        let decoded = HostDescriptor::new("missing.Source", serde_json::json!({ "expr": "*" }));
        let err = decoded
            .decode_as::<ScheduleSource>(&resources())
            .expect_err("unknown source type should fail");

        assert!(
            matches!(err, HostDescriptorError::UnknownSourceType { source_type } if source_type == "missing.Source")
        );
    }

    #[test]
    fn host_descriptor_typed_decode_reports_malformed_payload() {
        let decoded = HostDescriptor::new("cron.Schedule", serde_json::json!({ "expr": 1 }));
        let err = decoded
            .decode_as::<ScheduleSource>(&resources())
            .expect_err("malformed source payload should fail");

        assert!(
            matches!(err, HostDescriptorError::MalformedPayload { source_type, .. } if source_type == "cron.Schedule")
        );
    }

    #[test]
    fn trigger_compatibility_accepts_matching_event_mapping() {
        let artifact = linked_artifact(
            "process tick(event: cron.Tick) { finish event.fired_at }",
            resources(),
        );
        let definition = definition_for(&artifact, "tick");

        let compatibility = check_trigger_compatibility(TriggerCompatibilityRequest {
            artifact: &artifact,
            definition: &definition,
            source_type: "cron.Schedule",
            inputs: &event_input_template(),
        })
        .expect("trigger should be compatible");

        assert_eq!(compatibility.definition, definition);
        assert_eq!(compatibility.event_type.name(), "cron.Tick");
        assert_eq!(
            format_type_expr(&compatibility.resolved_event_type),
            "{ fired_at: str }"
        );
    }

    #[test]
    fn trigger_compatibility_rejects_unknown_source() {
        let artifact = linked_artifact(
            "process tick(event: cron.Tick) { finish event.fired_at }",
            resources(),
        );
        let definition = definition_for(&artifact, "tick");

        let err = check_trigger_compatibility(TriggerCompatibilityRequest {
            artifact: &artifact,
            definition: &definition,
            source_type: "missing.Source",
            inputs: &event_input_template(),
        })
        .expect_err("unknown source should fail");

        assert!(
            matches!(err, TriggerCompatibilityError::UnknownSourceType { source_type } if source_type == "missing.Source")
        );
    }

    #[test]
    fn trigger_compatibility_rejects_wrong_process_ref() {
        let artifact = linked_artifact(
            "process tick(event: cron.Tick) { finish event.fired_at }",
            resources(),
        );
        let mut definition = definition_for(&artifact, "tick");
        definition.process_ref.pos = definition.process_ref.pos.saturating_add(1);

        let err = check_trigger_compatibility(TriggerCompatibilityRequest {
            artifact: &artifact,
            definition: &definition,
            source_type: "cron.Schedule",
            inputs: &event_input_template(),
        })
        .expect_err("wrong process ref should fail");

        assert!(matches!(
            err,
            TriggerCompatibilityError::ProcessRefMismatch { .. }
        ));
    }

    #[test]
    fn trigger_compatibility_rejects_missing_event_input() {
        let artifact = linked_artifact(
            "process tick(event: cron.Tick) { finish event.fired_at }",
            resources(),
        );
        let definition = definition_for(&artifact, "tick");
        let inputs = TriggerInputTemplate::new(BTreeMap::from([(
            "event".to_string(),
            TriggerInputBinding::Fixed {
                value: serde_json::json!({"fired_at": "now"}),
            },
        )]));

        let err = check_trigger_compatibility(TriggerCompatibilityRequest {
            artifact: &artifact,
            definition: &definition,
            source_type: "cron.Schedule",
            inputs: &inputs,
        })
        .expect_err("missing event mapping should fail");

        assert!(matches!(
            err,
            TriggerCompatibilityError::MissingEventInput { .. }
        ));
    }

    #[test]
    fn trigger_compatibility_rejects_unknown_input() {
        let artifact = linked_artifact(
            "process tick(event: cron.Tick) { finish event.fired_at }",
            resources(),
        );
        let definition = definition_for(&artifact, "tick");
        let inputs = TriggerInputTemplate::new(BTreeMap::from([
            ("event".to_string(), TriggerInputBinding::Event),
            ("extra".to_string(), TriggerInputBinding::Event),
        ]));

        let err = check_trigger_compatibility(TriggerCompatibilityRequest {
            artifact: &artifact,
            definition: &definition,
            source_type: "cron.Schedule",
            inputs: &inputs,
        })
        .expect_err("unknown input should fail");

        assert!(matches!(
            err,
            TriggerCompatibilityError::UnknownInput { input, .. }
                if input == "extra"
        ));
    }

    #[test]
    fn trigger_compatibility_rejects_event_type_mismatch() {
        let artifact = linked_artifact(
            "process tick(event: Type { fired_at: str, required: str }) { finish event.fired_at }",
            resources(),
        );
        let definition = definition_for(&artifact, "tick");

        let err = check_trigger_compatibility(TriggerCompatibilityRequest {
            artifact: &artifact,
            definition: &definition,
            source_type: "cron.Schedule",
            inputs: &event_input_template(),
        })
        .expect_err("event mismatch should fail");

        assert!(matches!(
            err,
            TriggerCompatibilityError::EventMismatch { .. }
        ));
    }

    #[test]
    fn trigger_compatibility_rejects_fixed_resource_mismatch() {
        let mut resources = resources();
        resources.ensure_resource_type("Bucket");
        let artifact = linked_artifact(
            "process tick(event: cron.Tick, bucket: Bucket) { finish event.fired_at }",
            resources,
        );
        let definition = definition_for(&artifact, "tick");
        let inputs = TriggerInputTemplate::new(BTreeMap::from([
            ("event".to_string(), TriggerInputBinding::Event),
            (
                "bucket".to_string(),
                TriggerInputBinding::Fixed {
                    value: serde_json::json!({
                        "__resource__": true,
                        "type": "OtherBucket",
                        "alias": "wrong",
                    }),
                },
            ),
        ]));

        let err = check_trigger_compatibility(TriggerCompatibilityRequest {
            artifact: &artifact,
            definition: &definition,
            source_type: "cron.Schedule",
            inputs: &inputs,
        })
        .expect_err("resource mismatch should fail");

        assert!(matches!(
            err,
            TriggerCompatibilityError::FixedInputMismatch { input, .. }
                if input == "bucket"
        ));
    }
}
