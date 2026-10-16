//! The explicit source recorder, separate from build certification.

use lash_kernel_doc::FunctionRegistry;
use serde::Serialize;
use std::collections::BTreeMap;

use super::corpus::{Case, Corpus, NativeRecord, digest};
use super::{inputs, probe::answer};

impl Corpus {
    /// Drops superseded identities from the release still under construction.
    /// Surviving identities keep their cases and must still certify; this mode
    /// cannot bless changed answers under an existing identity. Shipped releases
    /// and their shared records are never replaced.
    pub(crate) fn replace_unsealed_baseline(
        &mut self,
        registry: &FunctionRegistry,
        dialect: &str,
        release: &str,
        sealed: &[&str],
    ) -> Result<(), String> {
        #[derive(serde::Deserialize)]
        struct ReleaseName {
            release: String,
        }
        for source in sealed {
            let shipped: ReleaseName =
                serde_json::from_str(source).map_err(|error| error.to_string())?;
            if shipped.release == release {
                return Err(format!(
                    "cannot replace native baseline for sealed release {dialect}@{release}"
                ));
            }
        }
        let mut replacement = self.clone();
        if let Some(index) = replacement
            .releases
            .get_mut(&format!("{dialect}@{release}"))
        {
            index.retain(|function, _| {
                registry
                    .get(function)
                    .is_some_and(|registered| registered.native().is_some())
            });
        }
        replacement.record_new(registry, dialect, release)?;
        *self = replacement;
        Ok(())
    }

    #[cfg_attr(
        not(test),
        expect(
            clippy::expect_used,
            reason = "the recorder serializes typed corpus data and modifies a release it just read"
        )
    )]
    fn intern(&mut self, value: &impl Serialize) -> String {
        let value = serde_json::to_value(value).expect("corpus data serializes");
        let id = digest(&value);
        self.blobs.entry(id.clone()).or_insert(value);
        id
    }

    /// Adds new identities after verifying every existing recorded identity.
    /// A new release reuses records of identities already held by another release.
    /// The release the tree builds is unsealed, so an identity it no longer
    /// holds natively leaves its index: the build certifies exactly the
    /// natives it holds.
    pub(crate) fn record_new(
        &mut self,
        registry: &FunctionRegistry,
        dialect: &str,
        release: &str,
    ) -> Result<(), String> {
        let key = format!("{dialect}@{release}");
        let mut index = self.releases.get(&key).cloned().unwrap_or_default();
        index.retain(|function, _| {
            registry
                .get(function)
                .is_some_and(|registered| registered.native().is_some())
        });
        for (function, registered) in registry
            .iter()
            .filter(|(_, registered)| registered.native().is_some())
        {
            if !index.contains_key(function) {
                let previous = self
                    .releases
                    .values()
                    .find_map(|index| index.get(function))
                    .cloned();
                let id = if let Some(id) = previous {
                    id
                } else {
                    let prepared = registered
                        .native()
                        .as_ref()
                        .and_then(|native| native.prepare());
                    let cases = inputs::generate(registered.definition(), &[])
                        .iter()
                        .map(|input| Case {
                            args: self.intern(&input.args),
                            objects: self.intern(&input.objects),
                            work: input.work,
                            room: input.room,
                            answer: self.intern(&answer(registered, input, prepared)),
                        })
                        .collect::<Vec<_>>();
                    let fingerprint = self.intern(&cases);
                    self.intern(&NativeRecord { fingerprint, cases })
                };
                index.insert(*function, id);
            }
        }
        self.releases.insert(key, index);
        self.certify(registry, dialect, release)?;
        Ok(())
    }

    /// Extends the corpus when generator policy grows; no old case or anchor
    /// can be removed or replaced. Certification runs before additions.
    pub(crate) fn extend_generated(
        &mut self,
        registry: &FunctionRegistry,
        dialect: &str,
        release: &str,
    ) -> Result<(), String> {
        self.certify(registry, dialect, release)?;
        for (function, registered) in registry
            .iter()
            .filter(|(_, registered)| registered.native().is_some())
        {
            self.append_inputs(
                registry,
                dialect,
                release,
                function,
                &inputs::generate(registered.definition(), &[]),
            )?;
        }
        Ok(())
    }

    #[cfg_attr(
        not(test),
        expect(
            clippy::expect_used,
            reason = "the recorder serializes typed corpus data and modifies a release it just read"
        )
    )]
    fn append_inputs(
        &mut self,
        registry: &FunctionRegistry,
        dialect: &str,
        release: &str,
        function: &lash_kernel_doc::FunctionId,
        inputs: &[super::probe::Input],
    ) -> Result<(), String> {
        let key = format!("{dialect}@{release}");
        let id = self.releases[&key][function].clone();
        let mut record: NativeRecord = self.read(&id)?;
        let registered = registry
            .get(function)
            .ok_or("the native is not registered")?;
        let mut seen: std::collections::BTreeSet<_> = record
            .cases
            .iter()
            .map(|case| {
                (
                    case.args.clone(),
                    case.objects.clone(),
                    case.work,
                    case.room,
                )
            })
            .collect();
        let prepared = registered
            .native()
            .as_ref()
            .and_then(|native| native.prepare());
        for input in inputs {
            let args = self.intern(&input.args);
            let objects = self.intern(&input.objects);
            if seen.insert((args.clone(), objects.clone(), input.work, input.room)) {
                record.cases.push(Case {
                    args,
                    objects,
                    work: input.work,
                    room: input.room,
                    answer: self.intern(&answer(registered, input, prepared)),
                });
            }
        }
        if record.cases.len() > super::corpus::MAX_CASES {
            return Err(format!("native {function} exceeds the corpus size bound"));
        }
        let id = self.intern(&record);
        self.releases
            .get_mut(&key)
            .expect("the release was just read")
            .insert(*function, id);
        Ok(())
    }

    /// Writes stable prefix shards: unchanged blobs and release records are
    /// shared once, even across dialects and retained releases.
    #[allow(
        clippy::disallowed_methods,
        reason = "the explicitly invoked native corpus recorder writes reviewed source data"
    )]
    pub(crate) fn write(
        &self,
        directory: &std::path::Path,
    ) -> Result<(), Box<dyn std::error::Error>> {
        use std::fmt::Write as _;
        let mut shards = BTreeMap::<&str, BTreeMap<&str, &serde_json::Value>>::new();
        let mut reachable = std::collections::BTreeSet::new();
        for index in self.releases.values() {
            for id in index.values() {
                reachable.insert(id.clone());
                let record: NativeRecord = self.read(id)?;
                for case in record.cases {
                    reachable.extend([case.args, case.objects, case.answer]);
                }
            }
        }
        for (id, blob) in &self.blobs {
            if reachable.contains(id) {
                shards.entry(&id[..2]).or_default().insert(id, blob);
            }
        }
        std::fs::create_dir_all(directory)?;
        let mut source =
            String::from("// @generated by the native corpus recorder; do not edit.\n");
        let _ = writeln!(
            source,
            "pub(crate) const RELEASES: &str = r####\"{}\"####;",
            serde_json::to_string_pretty(&self.releases)?
        );
        for prefix in shards.keys() {
            let _ = writeln!(source, "mod b{prefix};");
        }
        source.push_str("pub(crate) const BLOBS: &[&str] = &[\n");
        for (prefix, blobs) in shards {
            let _ = writeln!(source, "    b{prefix}::DATA,");
            let mut shard = String::from(
                "// @generated by the native corpus recorder; do not edit.\npub(super) const DATA: &str = r####\"{\n",
            );
            for (position, (id, value)) in blobs.into_iter().enumerate() {
                if position > 0 {
                    shard.push_str(",\n");
                }
                let _ = write!(
                    shard,
                    "{}:{}",
                    serde_json::to_string(id)?,
                    source_json(value)?
                );
            }
            shard.push_str("\n}\"####;\n");
            std::fs::write(directory.join(format!("b{prefix}.rs")), shard)?;
        }
        source.push_str("];\n");
        std::fs::write(directory.join("mod.rs"), source)?;
        Ok(())
    }
}

/// Keep invisible and bidirectional text as JSON escapes in Rust source. Decoding
/// yields the same input; its content address and expected answer are unchanged.
fn source_json(value: &serde_json::Value) -> Result<String, serde_json::Error> {
    let mut json = serde_json::to_string(value)?;
    for code in (0x202a..=0x202e)
        .chain(0x2066..=0x2069)
        .chain([0x00ad, 0x200b, 0x200c, 0x200d, 0x2060, 0xfeff])
    {
        if let Some(character) = char::from_u32(code) {
            json = json.replace(character, &format!("\\u{code:04x}"));
        }
    }
    Ok(json)
}

/// Read-only review data; this never changes a case or its acceptance rule.
#[derive(Serialize)]
pub(crate) struct ReplayDifference {
    pub(crate) release: String,
    pub(crate) name: String,
    pub(crate) function: lash_kernel_doc::FunctionId,
    pub(crate) record: String,
    pub(crate) case: usize,
    pub(crate) args: String,
    pub(crate) objects: String,
    pub(crate) work: u64,
    pub(crate) room: u64,
    pub(crate) old_answer_id: String,
    pub(crate) new_answer_id: String,
    pub(crate) old: super::probe::Answer,
    pub(crate) new: super::probe::Answer,
}
impl Corpus {
    pub(crate) fn audit(
        &self,
        registry: &FunctionRegistry,
        dialect: &str,
        release: &str,
    ) -> Result<Vec<ReplayDifference>, String> {
        let release = format!("{dialect}@{release}");
        let entries = self.releases.get(&release).ok_or("release is absent")?;
        let mut differences = Vec::new();
        for (function, id) in entries {
            let registered = registry
                .get(function)
                .ok_or("native implementation is absent")?;
            let record: NativeRecord = self.read(id)?;
            let prepared = registered.native().and_then(|native| native.prepare());
            for (index, case) in record.cases.iter().enumerate() {
                let input = super::probe::Input {
                    args: self.read(&case.args)?,
                    objects: self.read(&case.objects)?,
                    work: case.work,
                    room: case.room,
                };
                let replay = match super::probe::fixtures::Replay::load(&input) {
                    Ok(replay) => replay,
                    Err(error) => {
                        println!(
                            "{}",
                            serde_json::json!({"invalid_fixture":{
                                "release":release,"name":registered.definition().name.to_string(),"function":function,
                                "record":id,"case":index,"args":case.args,"objects":case.objects,"error":error
                            }})
                        );
                        continue;
                    }
                };
                let old = self.read(&case.answer)?;
                let new = super::probe::answer_loaded(registered, &input, prepared, replay);
                if old != new {
                    let value = serde_json::to_value(&new).map_err(|e| e.to_string())?;
                    differences.push(ReplayDifference {
                        release: release.clone(),
                        name: registered.definition().name.to_string(),
                        function: *function,
                        record: id.clone(),
                        case: index,
                        args: case.args.clone(),
                        objects: case.objects.clone(),
                        work: case.work,
                        room: case.room,
                        old_answer_id: case.answer.clone(),
                        new_answer_id: digest(&value),
                        old,
                        new,
                    });
                }
            }
        }
        Ok(differences)
    }
}
