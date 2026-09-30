// FIG-4374: a session's recorded model is minted by the host's registry when
// a key is selected, and recorded; host code cannot assemble one from a key
// and metadata of its choosing.

fn a_recorded_model_is_minted_by_the_registry_only(metadata: lash::ModelMetadata) {
    let _ = lash::RecordedModel {
        key: lash::ModelKey::new("forged"),
        metadata,
    };
}

fn main() {}
