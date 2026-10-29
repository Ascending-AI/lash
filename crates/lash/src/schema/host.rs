//! The checked-in host contracts, available without schema generation or
//! optional runtime features. Each module names its `schemas/host/` directory.
//! `JSON_SCHEMA` describes validation constraints; `TYPESCRIPT_DECLARATION`
//! is provided wherever the contract also ships TypeScript.
//!
//! Regenerate the crate-local documents with
//! `scripts/generate-workflow-schemas.py`.

macro_rules! host_schemas {
    ($($module:ident { $($name:ident: ($shipped:expr, $checked:expr)),+ $(,)? })+) => {
        $(
            #[doc = concat!("The `", stringify!($module), "` host contract.")]
            pub mod $module {
                $(
                    #[doc = concat!("The shipped `", stringify!($name), "` contract text.")]
                    pub const $name: &str = $shipped;
                )+
            }
        )+

        #[cfg(test)]
        mod tests {
            /// FIG-6699: every published host contract is byte-identical to
            /// its authoritative checked-in document.
            #[test]
            fn every_shipped_host_contract_equals_its_schema_file() {
                $( $(
                    assert_eq!(super::$module::$name, $checked,
                        "{}::{}", stringify!($module), stringify!($name));
                )+ )+
            }
        }
    };
}

host_schemas! {
    kernel_authoring_operation {
        TYPESCRIPT_DECLARATION: (
            include_str!("../../host-schemas/kernel-authoring-operation/v1.d.ts"),
            include_str!("../../../../schemas/host/kernel-authoring-operation/v1.d.ts")
        ),
        JSON_SCHEMA: (
            include_str!("../../host-schemas/kernel-authoring-operation/v1.schema.json"),
            include_str!("../../../../schemas/host/kernel-authoring-operation/v1.schema.json")
        ),
    }
    kernel_caption_view {
        TYPESCRIPT_DECLARATION: (
            include_str!("../../host-schemas/kernel-caption-view/v1.d.ts"),
            include_str!("../../../../schemas/host/kernel-caption-view/v1.d.ts")
        ),
        JSON_SCHEMA: (
            include_str!("../../host-schemas/kernel-caption-view/v1.schema.json"),
            include_str!("../../../../schemas/host/kernel-caption-view/v1.schema.json")
        ),
    }
    kernel_captions {
        TYPESCRIPT_DECLARATION: (
            include_str!("../../host-schemas/kernel-captions/v1.d.ts"),
            include_str!("../../../../schemas/host/kernel-captions/v1.d.ts")
        ),
        JSON_SCHEMA: (
            include_str!("../../host-schemas/kernel-captions/v1.schema.json"),
            include_str!("../../../../schemas/host/kernel-captions/v1.schema.json")
        ),
    }
    kernel_correspondence {
        JSON_SCHEMA: (
            include_str!("../../host-schemas/kernel-correspondence/v1.schema.json"),
            include_str!("../../../../schemas/host/kernel-correspondence/v1.schema.json")
        ),
    }
    kernel_document {
        JSON_SCHEMA: (
            include_str!("../../host-schemas/kernel-document/v1.schema.json"),
            include_str!("../../../../schemas/host/kernel-document/v1.schema.json")
        ),
    }
    kernel_edit_refusal {
        JSON_SCHEMA: (
            include_str!("../../host-schemas/kernel-edit-refusal/v1.schema.json"),
            include_str!("../../../../schemas/host/kernel-edit-refusal/v1.schema.json")
        ),
    }
    kernel_edit_transaction {
        JSON_SCHEMA: (
            include_str!("../../host-schemas/kernel-edit-transaction/v1.schema.json"),
            include_str!("../../../../schemas/host/kernel-edit-transaction/v1.schema.json")
        ),
    }
    kernel_entry_extraction {
        JSON_SCHEMA: (
            include_str!("../../host-schemas/kernel-entry-extraction/v1.schema.json"),
            include_str!("../../../../schemas/host/kernel-entry-extraction/v1.schema.json")
        ),
    }
    kernel_function {
        JSON_SCHEMA: (
            include_str!("../../host-schemas/kernel-function/v1.schema.json"),
            include_str!("../../../../schemas/host/kernel-function/v1.schema.json")
        ),
    }
    kernel_function_declarations {
        JSON_SCHEMA: (
            include_str!("../../host-schemas/kernel-function-declarations/v1.schema.json"),
            include_str!("../../../../schemas/host/kernel-function-declarations/v1.schema.json")
        ),
    }
    kernel_migration_refusal {
        TYPESCRIPT_DECLARATION: (
            include_str!("../../host-schemas/kernel-migration-refusal/v1.d.ts"),
            include_str!("../../../../schemas/host/kernel-migration-refusal/v1.d.ts")
        ),
        JSON_SCHEMA: (
            include_str!("../../host-schemas/kernel-migration-refusal/v1.schema.json"),
            include_str!("../../../../schemas/host/kernel-migration-refusal/v1.schema.json")
        ),
    }
    kernel_parked_refusal {
        TYPESCRIPT_DECLARATION: (
            include_str!("../../host-schemas/kernel-parked-refusal/v1.d.ts"),
            include_str!("../../../../schemas/host/kernel-parked-refusal/v1.d.ts")
        ),
        JSON_SCHEMA: (
            include_str!("../../host-schemas/kernel-parked-refusal/v1.schema.json"),
            include_str!("../../../../schemas/host/kernel-parked-refusal/v1.schema.json")
        ),
    }
    kernel_source_map {
        JSON_SCHEMA: (
            include_str!("../../host-schemas/kernel-source-map/v1.schema.json"),
            include_str!("../../../../schemas/host/kernel-source-map/v1.schema.json")
        ),
    }
    kernel_workflow_view {
        TYPESCRIPT_DECLARATION: (
            include_str!("../../host-schemas/kernel-workflow-view/v1.d.ts"),
            include_str!("../../../../schemas/host/kernel-workflow-view/v1.d.ts")
        ),
        JSON_SCHEMA: (
            include_str!("../../host-schemas/kernel-workflow-view/v1.schema.json"),
            include_str!("../../../../schemas/host/kernel-workflow-view/v1.schema.json")
        ),
    }
    postgres_host_config {
        JSON_SCHEMA: (
            include_str!("../../host-schemas/postgres-host-config/v1.schema.json"),
            include_str!("../../../../schemas/host/postgres-host-config/v1.schema.json")
        ),
    }
    process_effect_omissions {
        JSON_SCHEMA: (
            include_str!("../../host-schemas/process-effect-omissions/v1.schema.json"),
            include_str!("../../../../schemas/host/process-effect-omissions/v1.schema.json")
        ),
    }
    process_effect_outcome {
        JSON_SCHEMA: (
            include_str!("../../host-schemas/process-effect-outcome/v1.schema.json"),
            include_str!("../../../../schemas/host/process-effect-outcome/v1.schema.json")
        ),
    }
    process_ran {
        JSON_SCHEMA: (
            include_str!("../../host-schemas/process-ran/v1.schema.json"),
            include_str!("../../../../schemas/host/process-ran/v1.schema.json")
        ),
    }
    process_workflow_overlay {
        JSON_SCHEMA: (
            include_str!("../../host-schemas/process-workflow-overlay/v1.schema.json"),
            include_str!("../../../../schemas/host/process-workflow-overlay/v1.schema.json")
        ),
    }
    turn_activity {
        TYPESCRIPT_DECLARATION: (
            include_str!("../../host-schemas/turn-activity/v2.d.ts"),
            include_str!("../../../../schemas/host/turn-activity/v2.d.ts")
        ),
        JSON_SCHEMA: (
            include_str!("../../host-schemas/turn-activity/v2.schema.json"),
            include_str!("../../../../schemas/host/turn-activity/v2.schema.json")
        ),
    }
    trace_record {
        TYPESCRIPT_DECLARATION: (
            include_str!("../../host-schemas/trace-record/v37.d.ts"),
            include_str!("../../../../schemas/host/trace-record/v37.d.ts")
        ),
        JSON_SCHEMA: (
            include_str!("../../host-schemas/trace-record/v37.schema.json"),
            include_str!("../../../../schemas/host/trace-record/v37.schema.json")
        ),
    }
    worker_page_reply {
        JSON_SCHEMA: (
            include_str!("../../host-schemas/worker-page-reply/v4.schema.json"),
            include_str!("../../../../schemas/host/worker-page-reply/v4.schema.json")
        ),
    }
    worker_page_request {
        JSON_SCHEMA: (
            include_str!("../../host-schemas/worker-page-request/v4.schema.json"),
            include_str!("../../../../schemas/host/worker-page-request/v4.schema.json")
        ),
    }
    worker_seal_batch {
        JSON_SCHEMA: (
            include_str!("../../host-schemas/worker-seal-batch/v4.schema.json"),
            include_str!("../../../../schemas/host/worker-seal-batch/v4.schema.json")
        ),
    }
    worker_start {
        JSON_SCHEMA: (
            include_str!("../../host-schemas/worker-start/v4.schema.json"),
            include_str!("../../../../schemas/host/worker-start/v4.schema.json")
        ),
    }
    workflow_execution_overlay {
        JSON_SCHEMA: (
            include_str!("../../host-schemas/workflow-execution-overlay/v37.schema.json"),
            include_str!("../../../../schemas/host/workflow-execution-overlay/v37.schema.json")
        ),
    }
}
