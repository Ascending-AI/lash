pub use lash_vocabulary::sites::*;
#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph_tokens::{TokenCodec, encode_body};
    use serde::Serialize;
    use serde::de::DeserializeOwned;

    /// FIG-6711: host JSON carries typed opaque tokens, never engine records;
    /// the binary checkpoint/admission identity encoding is unchanged.
    #[test]
    fn host_identity_tokens_are_opaque_and_preserve_binary_identity() {
        fn token<T: TokenCodec + Serialize + DeserializeOwned + PartialEq + std::fmt::Debug>(
            value: T,
            baseline: Vec<u8>,
            wrong_kind: serde_json::Value,
        ) {
            let json = serde_json::to_value(&value).unwrap();
            assert!(json.is_string());
            assert_eq!(serde_json::from_value::<T>(json).unwrap(), value);
            assert!(serde_json::from_value::<T>(wrong_kind).is_err());
            assert_eq!(encode_body(&value), baseline);
        }
        let task = TaskIdentity::Main;
        let site = Site::new(crate::Unit::Main, vec![2]);
        let effect = EffectIdentity {
            task: task.clone(),
            site: site.clone(),
            occurrence: 3,
            loops: vec![],
        };
        token(
            TaskLabel(task.clone()),
            encode_body(&task),
            serde_json::json!("w:0300"),
        );
        token(
            PositionLabel(site.clone()),
            encode_body(&site),
            serde_json::json!({ "unit": "main", "path": [2] }),
        );
        token(
            EffectId(effect.clone()),
            encode_body(&effect),
            serde_json::to_value(&effect).unwrap(),
        );
        token(VisitId(3), encode_body(&3_u64), serde_json::json!(3));
        token(WaitId(3), encode_body(&3_u64), serde_json::json!(3));
    }
}
