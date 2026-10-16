//! The live fuzzer and the hermetic laws share exactly this machine adapter.
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use lash_kernel_dialect::{Environment, NamedLibrary};
use lash_kernel_doc::{
    Datum, EffectName, ErrorDatum, FunctionRegistry, Handle, Integer, Name, Param, Signature,
    Timestamp, Type,
};
use lash_kernel_vm::{
    Bounds, CellStart, End, Host, Layout, Machine, Outcome, PreparedLibrary, Program, Request,
    RunError, Step, Target,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Clone, Deserialize, Serialize)]
pub struct Case {
    pub id: String,
    pub dialect: String,
    pub source: String,
    #[serde(default)]
    pub bits: u64,
    #[serde(default)]
    pub stride: u64,
    #[serde(default)]
    pub nanoseconds: i64,
}

#[derive(Debug, Serialize)]
pub struct Observed {
    pub result: Value,
    pub reached: BTreeSet<String>,
    /// What the run was charged.
    pub charged: u64,
}

/// A case lowered and admitted, or what refusing to lower it observes.
pub enum Lowered {
    Program(Program),
    Refused(Observed),
}

pub struct Runner {
    library: NamedLibrary,
    prepared: PreparedLibrary,
}

impl Runner {
    pub fn new(dialect: &str) -> Result<Self, String> {
        let mut registry = FunctionRegistry::new();
        lash_kernel_vm::register_machine_functions(&mut registry).map_err(|e| e.to_string())?;
        lash_kernel_lib::register_numbers(&mut registry).map_err(|e| e.to_string())?;
        lash_kernel_lib::register_collections(&mut registry).map_err(|e| e.to_string())?;
        lash_kernel_lib::register_text_json(&mut registry).map_err(|e| e.to_string())?;
        if dialect == "typescript" {
            lash_ext_date_ecma::register(&mut registry).map_err(|e| e.to_string())?;
            lash_ext_url_whatwg::register(&mut registry).map_err(|e| e.to_string())?;
            lash_ext_regex_ecma::register(
                &mut registry,
                &Arc::new(lash_ext_regex_ecma::Engine::new(32)),
            )
            .map_err(|e| e.to_string())?;
        }
        let mut library = NamedLibrary::from_registry(&registry).map_err(|e| e.to_string())?;
        let definitions = match dialect {
            "typescript" => lash_dialect_typescript::define_helpers(&mut library),
            "python" => lash_dialect_python::define_helpers(&mut library),
            _ => return Err(format!("unknown dialect {dialect}")),
        }
        .map_err(|e| e.to_string())?;
        let mut helpers = Vec::new();
        for definition in definitions {
            helpers.push(
                registry
                    .register(definition, None)
                    .map_err(|e| e.to_string())?,
            );
        }
        // The runner installs each dialect's operation natives, as a worker
        // does (FIG-6440).
        match dialect {
            "typescript" => {
                lash_ext_ts_core::implement(&mut registry, helpers).map_err(|e| e.to_string())?
            }
            _ => lash_ext_py_core::implement(&mut registry, helpers).map_err(|e| e.to_string())?,
        }
        Ok(Self {
            library,
            prepared: PreparedLibrary::new(Arc::new(registry)),
        })
    }

    pub fn run(&self, case: &Case) -> Result<Observed, String> {
        match self.lower(case)? {
            Lowered::Program(program) => self.execute(case, &program, &[Layout::default()]),
            Lowered::Refused(observed) => Ok(observed),
        }
    }

    /// Lowers and admits `case`, or what a refusal to lower it observes.
    pub fn lower(&self, case: &Case) -> Result<Lowered, String> {
        let effects = BTreeMap::from([(
            EffectName::new("observe").map_err(|e| e.to_string())?,
            Signature {
                params: vec![Param {
                    name: Name::new("value"),
                    ty: Type::Any,
                    optional: false,
                }],
                result: Type::Any,
            },
        )]);
        let env = Environment {
            library: &self.library,
            effects: &effects,
            tool_roots: &BTreeSet::new(),
            controls: &BTreeMap::new(),
        };
        let lowered = match case.dialect.as_str() {
            "typescript" => lash_dialect_typescript::lower(
                &case.source,
                &env,
                lash_kernel_dialect::Mode::Cell(lash_kernel_dialect::CellEnvironment::first()),
            )
            .map_err(|e| e.to_string()),
            "python" => lash_dialect_python::lower(
                &case.source,
                &env,
                lash_kernel_dialect::Mode::Cell(lash_kernel_dialect::CellEnvironment::first()),
            )
            .map_err(|e| e.to_string()),
            _ => return Err("unknown dialect".into()),
        };
        let lowered = match lowered {
            Ok(lowered) => lowered,
            Err(message) => {
                return Ok(Lowered::Refused(Observed {
                    result: json!({"type":"refusal", "value":message.split(':').next().unwrap_or(&message)}),
                    reached: BTreeSet::new(),
                    charged: 0,
                }));
            }
        };
        let mut admission = lash_kernel_check::Environment::new(&self.library);
        admission.effects = effects;
        lash_kernel_check::admit(&lowered.document, &admission).map_err(|e| format!("{e:?}"))?;
        Ok(Lowered::Program(Program {
            release: crate::pages::RELEASE,
            document: Arc::new(lowered.document),
            library: self.prepared.clone(),
        }))
    }

    /// Runs `program`, lowered from `case`, built in the first of
    /// `layouts`. Given more than one, the run is sealed at each park and
    /// opened cold in the next of them, by turns, before the park's
    /// requests are answered.
    pub fn execute(
        &self,
        case: &Case,
        program: &Program,
        layouts: &[Layout],
    ) -> Result<Observed, String> {
        let reopen = layouts.len() > 1;
        let mut layouts = layouts.iter().copied().cycle();
        let first = layouts.next().ok_or("a plan names a layout")?;
        let bounds = Bounds {
            charge: 50_000_000,
            memory: 64 << 20,
            call_depth: 200,
            live_tasks: 100,
            requests_per_park: 100,
            join_members: 100,
        };
        let source = crate::pages::Source::default();
        let mut machine = crate::pages::with_source(
            program.clone(),
            bounds,
            CellStart {
                target: Target::Main,
                args: vec![],
                session: None,
                owner: crate::pages::OWNER,
                modes: Default::default(),
                host: Vec::new(),
            },
            first,
            &source,
        )
        .map_err(|e| e.to_string())?;
        machine.count_work();
        let mut reached = BTreeSet::new();
        let mut host = ScriptHost {
            bits: case.bits,
            stride: case.stride,
            nanoseconds: case.nanoseconds,
        };
        let mut values = vec![];
        let result = loop {
            match machine
                .run(&mut host, u64::MAX)
                .map_err(|e| e.to_string())?
            {
                Step::Slice => return Err("unbounded slice yielded".into()),
                Step::Parked(park) => {
                    if reopen {
                        reached.extend(Self::reached(&machine)?);
                        let charged = machine.meters().charged;
                        let layout = layouts.next().ok_or("a plan names a layout")?;
                        crate::pages::reopen(&mut machine, program, bounds, &source, layout);
                        machine.count_work();
                        if machine.meters().charged != charged {
                            return Err("reopening changed the charge".into());
                        }
                    }
                    for request in park.requests {
                        let effect = match request {
                            Request::Effect(effect) => effect,
                            Request::Sleep(sleep) => {
                                machine
                                    .deliver(sleep.wait, Outcome::Elapsed)
                                    .map_err(|e| e.to_string())?;
                                continue;
                            }
                        };
                        if effect.effect.as_str() != "observe" {
                            return Err("unexpected effect".into());
                        }
                        let value = effect.args.first().ok_or("observe needs a value")?;
                        values.push(tag(value));
                        machine
                            .deliver(effect.wait, Outcome::Completed(Datum::Null))
                            .map_err(|e| e.to_string())?;
                    }
                }
                Step::Ended(End::Finished(_)) => {
                    break json!({"type":"observations", "value":values});
                }
                Step::Ended(End::Error(RunError::Uncaught(Datum::Error(error)))) => {
                    break json!({"type":"throw", "value":error_class(&error.kind)});
                }
                Step::Ended(End::Error(RunError::Uncaught(value))) => {
                    break json!({"type":"throw_value", "value":tag(&value)});
                }
                Step::Ended(other) => return Err(format!("unexpected end: {other:?}")),
            }
        };
        reached.extend(Self::reached(&machine)?);
        Ok(Observed {
            result,
            reached,
            charged: machine.meters().charged,
        })
    }

    /// The library functions whose code or native ran in `machine`.
    fn reached(machine: &lash_kernel_vm::KernelMachine) -> Result<BTreeSet<String>, String> {
        let work = machine.work().ok_or("work counting is enabled")?;
        let mut reached: BTreeSet<String> = work
            .codes
            .into_iter()
            .filter(|row| row.counts.calls > 0 && row.symbol.function.is_some())
            .map(|row| row.symbol.name)
            .collect();
        reached.extend(
            work.natives
                .into_iter()
                .filter(|row| row.counts.calls > 0)
                .map(|row| row.symbol.name),
        );
        Ok(reached)
    }
}

struct ScriptHost {
    bits: u64,
    stride: u64,
    nanoseconds: i64,
}
impl Host for ScriptHost {
    fn clock(&mut self) -> Timestamp {
        Timestamp {
            nanoseconds: Integer::from(self.nanoseconds),
        }
    }
    fn random(&mut self) -> u64 {
        let bits = self.bits;
        self.bits = bits.wrapping_add(self.stride);
        bits
    }
    fn read(&mut self, _: &Handle, _: &Datum) -> Result<Datum, ErrorDatum> {
        Err(ErrorDatum {
            kind: "type_error".into(),
            message: "no projection".into(),
            data: Datum::Null,
        })
    }
    fn print(&mut self, _: &Datum) {}
    fn cancel_requested(&mut self) -> bool {
        false
    }
}

fn error_class(kind: &str) -> &str {
    match kind {
        "type_error" => "TypeError",
        "range_error" => "RangeError",
        other => other,
    }
}

pub fn tag(value: &Datum) -> Value {
    match value {
        Datum::Null => json!({"type":"null"}),
        Datum::Absent => json!({"type":"undefined"}),
        Datum::Bool(value) => json!({"type":"bool", "value":value}),
        Datum::Int(value) => json!({"type":"int", "value":value.to_string()}),
        Datum::Float(value) => {
            json!({"type":"float", "bits":format!("{:016x}", value.get().to_bits())})
        }
        Datum::Text(value) => {
            json!({"type":"str", "value":value.encode_utf16().collect::<Vec<_>>()})
        }
        Datum::Tuple(values) => {
            json!({"type":"tuple", "value":values.iter().map(tag).collect::<Vec<_>>()})
        }
        Datum::List(values) => {
            json!({"type":"list", "value":values.iter().map(tag).collect::<Vec<_>>()})
        }
        Datum::Set(values) => {
            json!({"type":"set", "value":values.iter().map(tag).collect::<Vec<_>>()})
        }
        Datum::Map(entries) => {
            json!({"type":"dict", "value":entries.iter().map(|(k,v)| vec![tag(k),tag(v)]).collect::<Vec<_>>()})
        }
        Datum::Record(entries) => {
            // ECMAScript own-key order at the language/host boundary. Programs
            // testing Object.keys still observe the dialect helper itself.
            let mut fields: Vec<_> = entries.iter().collect();
            fields.sort_by_key(|(key, _)| {
                key.parse::<u32>()
                    .ok()
                    .filter(|n| *n != u32::MAX && n.to_string() == *key)
            });
            let (indices, others): (Vec<_>, Vec<_>) = fields.into_iter().partition(|(key, _)| {
                key.parse::<u32>()
                    .ok()
                    .is_some_and(|n| n != u32::MAX && n.to_string() == *key)
            });
            json!({"type":"object", "value":indices.into_iter().chain(others).map(|(k,v)| json!([k,tag(v)])).collect::<Vec<_>>()})
        }
        other => json!({"type":"unsupported", "value":format!("{other:?}")}),
    }
}

/// The one census seam: the generated closed helper set, checked against
/// the current release writes (which also carry kernel/extension natives).
#[cfg(test)]
pub fn helper_writes(dialect: &str) -> Result<BTreeSet<String>, String> {
    use lash_vm_releases::{PythonHelper, TypeScriptHelper};
    let helpers: BTreeSet<String> = match dialect {
        "typescript" => TypeScriptHelper::ALL
            .iter()
            .map(|helper| helper.name().to_owned())
            .collect(),
        "python" => PythonHelper::ALL
            .iter()
            .map(|helper| helper.name().to_owned())
            .collect(),
        _ => return Err(format!("unknown dialect {dialect}")),
    };
    let built = lash_vm_releases::dialect(dialect).ok_or("missing dialect release")?;
    let held = built.held_releases()?;
    let release = held.retained.last().ok_or("missing current release")?;
    let writes: BTreeSet<String> = release
        .functions
        .iter()
        .filter(|function| release.writes.contains(&function.function))
        .map(|function| function.definition.name.to_string())
        .collect();
    if !helpers.is_subset(&writes) {
        return Err("generated helpers are missing from current release writes".into());
    }
    Ok(helpers)
}
