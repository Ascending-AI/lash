use super::*;

#[test]
fn try_extend_composes_a_module_instance_without_operations() {
    let mut base = LashlangHostCatalog::new();
    let mut incoming = LashlangHostCatalog::new();
    incoming
        .add_module_instance(["directory"], "Directory")
        .expect("module instance is unique");

    base.try_extend(incoming)
        .expect("an empty-operation module instance composes");

    assert!(base.resolve_module_path(&["directory"]).is_some());
    assert!(base.has_resource_type("Directory"));
}

#[test]
fn try_extend_refuses_a_duplicate_module_operation() {
    let mut catalog = LashlangHostCatalog::new();
    catalog
        .add_module_operation(
            ["directory"],
            "Directory",
            "lookup",
            "first",
            TypeExpr::Any,
            TypeExpr::Any,
        )
        .expect("first binding is valid");
    let mut incoming = LashlangHostCatalog::new();
    incoming
        .add_module_operation(
            ["directory"],
            "Directory",
            "lookup",
            "second",
            TypeExpr::Any,
            TypeExpr::Any,
        )
        .expect("incoming binding is valid in isolation");
    let direct_error = catalog
        .clone()
        .add_module_operation(
            ["directory"],
            "Directory",
            "lookup",
            "second",
            TypeExpr::Any,
            TypeExpr::Any,
        )
        .expect_err("direct registration must refuse the duplicate");

    assert_eq!(
        catalog
            .try_extend(incoming)
            .expect_err("catalog composition must refuse the duplicate"),
        direct_error
    );
    assert_eq!(
        direct_error,
        LashlangHostCatalogError::ConflictingModuleOperation {
            module: "directory".to_string(),
            operation: "lookup".to_string(),
            existing: "first".to_string(),
            incoming: "second".to_string(),
        }
    );
}

#[test]
fn try_extend_refuses_a_duplicate_resource_operation() {
    let mut catalog = LashlangHostCatalog::new();
    catalog
        .add_operation("Directory", "lookup", TypeExpr::Any, TypeExpr::Str)
        .expect("first resource operation");
    let mut incoming = LashlangHostCatalog::new();
    incoming
        .add_operation("Directory", "lookup", TypeExpr::Any, TypeExpr::Int)
        .expect("incoming resource operation");
    let direct_error = catalog
        .clone()
        .add_operation("Directory", "lookup", TypeExpr::Any, TypeExpr::Int)
        .expect_err("direct registration must refuse the duplicate");

    assert_eq!(
        catalog
            .try_extend(incoming)
            .expect_err("resource operation composition must refuse the duplicate"),
        direct_error
    );
    assert_eq!(
        direct_error,
        LashlangHostCatalogError::ConflictingResourceOperation {
            resource_type: "Directory".to_string(),
            operation: "lookup".to_string(),
        }
    );
}

#[test]
fn try_extend_refuses_a_duplicate_value_constructor() {
    let mut catalog = LashlangHostCatalog::new();
    catalog
        .add_value_constructor(
            ["directory", "Entry"],
            TypeExpr::Str,
            TypeExpr::Ref("directory.Entry".into()),
        )
        .expect("first value constructor");
    let mut incoming = LashlangHostCatalog::new();
    incoming
        .add_value_constructor(
            ["directory", "Entry"],
            TypeExpr::Int,
            TypeExpr::Ref("directory.Entry".into()),
        )
        .expect("incoming value constructor");
    let direct_error = catalog
        .clone()
        .add_value_constructor(
            ["directory", "Entry"],
            TypeExpr::Int,
            TypeExpr::Ref("directory.Entry".into()),
        )
        .expect_err("direct registration must refuse the duplicate");

    assert_eq!(
        catalog
            .try_extend(incoming)
            .expect_err("value constructor composition must refuse the duplicate"),
        direct_error
    );
    assert_eq!(
        direct_error,
        LashlangHostCatalogError::ConflictingValueConstructor {
            path: "directory.Entry".to_string(),
        }
    );
}

#[test]
fn try_extend_refuses_an_identical_named_data_type() {
    let mut catalog = LashlangHostCatalog::new();
    catalog
        .add_named_data_type(timer_tick_type_with_field("fired_at"))
        .expect("first definition");
    let mut incoming = LashlangHostCatalog::new();
    incoming
        .add_named_data_type(timer_tick_type_with_field("fired_at"))
        .expect("incoming definition is valid in isolation");
    let direct_error = catalog
        .clone()
        .add_named_data_type(timer_tick_type_with_field("fired_at"))
        .expect_err("direct registration must refuse the duplicate");

    assert_eq!(
        catalog
            .try_extend(incoming)
            .expect_err("duplicate names are refused even when definitions are equal"),
        direct_error
    );
    assert_eq!(
        direct_error,
        LashlangHostCatalogError::ConflictingNamedDataType {
            name: "timer.Tick".to_string(),
        }
    );
}

#[test]
fn raw_operation_registration_cannot_half_register_a_module_operation() {
    let mut catalog = LashlangHostCatalog::new();
    catalog
        .add_module_instance(["directory"], "Directory")
        .expect("module instance is unique");

    assert_eq!(
        catalog
            .add_operation("Directory", "lookup", TypeExpr::Any, TypeExpr::Any)
            .expect_err("a module operation requires its host dispatch binding"),
        LashlangHostCatalogError::UnboundModuleOperation {
            module: "directory".to_string(),
            resource_type: "Directory".to_string(),
            operation: "lookup".to_string(),
        }
    );
    assert!(catalog.resolve_operation("Directory", "lookup").is_none());
    assert!(
        catalog
            .resolve_module_operation("Directory", "directory", "lookup")
            .is_none()
    );
}

#[test]
fn joint_resolution_rejects_a_decoded_half_operation() {
    let catalog: LashlangHostCatalog = serde_json::from_value(serde_json::json!({
        "module_instances": {
            "directory": {
                "path": ["directory"],
                "resource_type": "Directory",
                "alias": "directory",
                "operations": {
                    "lookup": { "host_operation": "directory.lookup" }
                }
            }
        },
        "resource_types": {
            "Directory": {}
        }
    }))
    .expect("the preserved serde shape permits an old half operation");

    assert!(
        catalog
            .resolve_module_operation("Directory", "directory", "lookup")
            .is_none(),
        "joint resolution must require both dispatch and signature"
    );
}
