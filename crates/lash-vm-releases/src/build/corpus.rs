//! Recorded native answer certification (FIG-6182, K-LIB-011).
//!
//! Release indexes refer to content-addressed native records; records refer to
//! shared arguments, heap snapshots and answers. A compatible implementation
//! returns the recorded manifest anchor, including the imported 1.0 anchors.
//! No implementation source or prepared-operation description is identified.

use std::collections::BTreeMap;

use lash_kernel_doc::{FunctionId, FunctionRegistry};
use serde::{Deserialize, Serialize};

pub(crate) const MAX_CASES: usize = 1024;
use super::probe::{Answer, Input, answer};

#[path = "corpus_data/mod.rs"]
#[cfg(not(test))]
mod recorded;
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Corpus {
    pub releases: BTreeMap<String, BTreeMap<FunctionId, String>>,
    pub blobs: BTreeMap<String, serde_json::Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeRecord {
    /// Opaque manifest anchor. Never recomputed or replaced on a match.
    pub(crate) fingerprint: String,
    pub(crate) cases: Vec<Case>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Case {
    pub(crate) args: String,
    pub(crate) objects: String,
    pub(crate) work: u64,
    pub(crate) room: u64,
    pub(crate) answer: String,
}

impl Corpus {
    #[cfg(not(test))]
    pub(crate) fn recorded() -> Result<Self, String> {
        let mut corpus = Self {
            releases: serde_json::from_str(recorded::RELEASES)
                .map_err(|error| error.to_string())?,
            ..Default::default()
        };
        for shard in recorded::BLOBS {
            // A 65-level immutable tuple uses more JSON wrapper levels than
            // serde's default bound. These are the build's own recorded values.
            let mut decoder = serde_json::Deserializer::from_str(shard);
            decoder.disable_recursion_limit();
            let blobs = BTreeMap::<String, serde_json::Value>::deserialize(&mut decoder)
                .map_err(|error| error.to_string())?;
            decoder.end().map_err(|error| error.to_string())?;
            for (id, value) in blobs {
                if corpus.blobs.insert(id.clone(), value).is_some() {
                    return Err(format!("corpus blob {id} is kept twice"));
                }
            }
        }
        for (id, value) in &corpus.blobs {
            if &digest(value) != id {
                return Err(format!("native corpus blob {id} has other content"));
            }
        }
        Ok(corpus)
    }

    pub(crate) fn read<T: serde::de::DeserializeOwned>(&self, id: &str) -> Result<T, String> {
        let value = self
            .blobs
            .get(id)
            .ok_or_else(|| format!("native corpus is missing blob {id}"))?;
        serde_json::from_value(value.clone())
            .map_err(|error| format!("native corpus blob {id}: {error}"))
    }

    fn input(&self, case: &Case) -> Result<Input, String> {
        Ok(Input {
            args: self.read(&case.args)?,
            objects: self.read(&case.objects)?,
            work: case.work,
            room: case.room,
        })
    }

    /// Verifies the current registry is completely recorded, then replays it.
    pub(crate) fn certify(
        &self,
        registry: &FunctionRegistry,
        dialect: &str,
        release: &str,
    ) -> Result<BTreeMap<FunctionId, String>, String> {
        let key = format!("{dialect}@{release}");
        let pinned = self
            .releases
            .get(&key)
            .ok_or_else(|| format!("helper release {key} has no native corpus"))?;
        for (function, registered) in registry
            .iter()
            .filter(|(_, registered)| registered.native.is_some())
        {
            if !pinned.contains_key(function) {
                return Err(format!(
                    "helper release {key}: native `{}` ({function}) has no recorded corpus; record its new identity with the native_corpus example before building the release",
                    registered.definition.name
                ));
            }
        }
        self.verify_release(registry, dialect, release)
    }

    /// Replays every pinned identity's recorded inputs, including retained
    /// releases whose native implementations the build must still hold.
    /// The standalone recorder shares this certification before writing.
    #[cfg_attr(
        not(test),
        expect(
            clippy::expect_used,
            reason = "the typed corpus values and JSON nodes have infallible JSON serialization"
        )
    )]
    pub(crate) fn verify_release(
        &self,
        registry: &FunctionRegistry,
        dialect: &str,
        release: &str,
    ) -> Result<BTreeMap<FunctionId, String>, String> {
        let key = format!("{dialect}@{release}");
        let pinned = self
            .releases
            .get(&key)
            .ok_or_else(|| format!("helper release {key} has no native corpus"))?;
        let mut fingerprints = BTreeMap::new();
        for (function, id) in pinned {
            let registered = registry.get(function).filter(|registered| registered.native.is_some())
                .ok_or_else(|| format!("helper release {key}: recorded native {function} has no retained implementation"))?;
            let record: NativeRecord = self.read(id)?;
            if record.cases.is_empty() || record.cases.len() > MAX_CASES {
                return Err(format!(
                    "native `{}` has an empty or oversized corpus",
                    registered.definition.name
                ));
            }
            let prepared = registered
                .native
                .as_ref()
                .and_then(|native| native.prepare());
            for case in &record.cases {
                let input = self.input(case)?;
                let old: Answer = self.read(&case.answer)?;
                let new = answer(registered, &input, prepared);
                if old != new {
                    return Err(format!(
                        "helper release {key}: native `{}` ({function}), input {} (permanent seed {}), work={} room={}: old {}; new {}. Fix the regression or state a new native version and retain the old implementation for parked runs.",
                        registered.definition.name,
                        serde_json::to_string(&input).expect("input JSON"),
                        case.args,
                        input.work,
                        input.room,
                        serde_json::to_string(&old).expect("answer JSON"),
                        serde_json::to_string(&new).expect("answer JSON")
                    ));
                }
            }
            fingerprints.insert(*function, record.fingerprint);
        }
        Ok(fingerprints)
    }
}

#[cfg_attr(
    not(test),
    expect(
        clippy::expect_used,
        reason = "the typed corpus values and JSON nodes have infallible JSON serialization"
    )
)]
pub(crate) fn digest(value: &serde_json::Value) -> String {
    blake3::hash(&serde_json::to_vec(value).expect("corpus JSON"))
        .to_hex()
        .to_string()
}
