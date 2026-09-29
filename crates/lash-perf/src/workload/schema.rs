use super::WorkloadSpec;
use schemars::{
    JsonSchema,
    r#gen::SchemaGenerator,
    schema::{RootSchema, Schema},
};
use std::collections::BTreeSet;

pub(super) fn positive_rate_schema(generator: &mut SchemaGenerator) -> Schema {
    let mut schema = f64::json_schema(generator).into_object();
    schema.number().exclusive_minimum = Some(0.0);
    Schema::Object(schema)
}

pub(super) fn confidence_schema(generator: &mut SchemaGenerator) -> Schema {
    let mut schema = positive_rate_schema(generator).into_object();
    schema.number().exclusive_maximum = Some(1.0);
    Schema::Object(schema)
}

pub(super) fn saturation_schema(generator: &mut SchemaGenerator) -> Schema {
    let mut schema = Vec::<f64>::json_schema(generator).into_object();
    schema.array().min_items = Some(1);
    schema.array().items = Some(positive_rate_schema(generator).into());
    Schema::Object(schema)
}

pub(super) fn generate() -> RootSchema {
    let mut schema = schemars::schema_for!(WorkloadSpec);
    let mut paths = BTreeSet::new();
    paths_from_schema(
        &Schema::Object(schema.schema.clone()),
        "",
        &schema,
        &mut paths,
    );
    let mut evidence = String::json_schema(&mut SchemaGenerator::default()).into_object();
    evidence.string().pattern = Some("^[IV](: .+)?$".into());
    if let Some(Schema::Object(provenance)) = schema.definitions.get_mut("Provenance")
        && let Some(fields) = provenance.object().properties.get_mut("fields")
    {
        let mut closed = fields.clone().into_object();
        closed.object().additional_properties = Some(Box::new(Schema::Bool(false)));
        closed.object().required = paths.clone();
        closed.object().properties = paths
            .into_iter()
            .map(|path| (path, Schema::Object(evidence.clone())))
            .collect();
        *fields = Schema::Object(closed);
    }
    schema
}

fn paths_from_schema(value: &Schema, path: &str, root: &RootSchema, paths: &mut BTreeSet<String>) {
    if let Schema::Object(object) = value {
        if let Some(reference) = &object.reference
            && let Some(name) = reference.strip_prefix("#/definitions/")
            && let Some(resolved) = root.definitions.get(name)
        {
            paths_from_schema(resolved, path, root, paths);
            return;
        }
        if let Some(object) = &object.object {
            for (key, child) in &object.properties {
                if key != "provenance" {
                    paths_from_schema(child, &format!("{path}/{key}"), root, paths);
                }
            }
            return;
        }
    }
    paths.insert(path.to_owned());
}
