//! The live fuzzer and the hermetic laws share exactly this machine adapter.
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use lash_kernel_dialect::{Environment, NamedLibrary};
use lash_kernel_doc::Datum;
use lash_kernel_doc::ErrorDatum;
use lash_kernel_doc::Handle;
use lash_kernel_doc::Integer;
use lash_kernel_doc::Timestamp;
use lash_kernel_vm::{
    Bounds, CellStart, End, Host, Machine, Outcome, PreparedLibrary, Program, Request, RunError,
    Step, Target,
};
use lash_vocabulary::names::EffectName;
use lash_vocabulary::names::Name;
use lash_vocabulary::types::Param;
use lash_vocabulary::types::Signature;
use lash_vocabulary::types::Type;
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
    /// Helpers named by the admitted document, independent of its execution plan.
    pub helpers: BTreeSet<String>,
    /// What the run was charged.
    pub charged: Option<u64>,
}

/// A case lowered and admitted, or what refusing to lower it observes.
pub enum Lowered {
    Program(Program),
    Refused(Observed),
}

pub struct Runner {
    library: NamedLibrary,
    helper_names: BTreeMap<lash_vocabulary::QualifiedName, String>,
    prepared: PreparedLibrary,
    charge: lash_kernel_vm::Charge,
}

impl Runner {
    pub fn new(dialect: &str) -> Result<Self, String> {
        Self::new_with_charge(dialect, lash_kernel_vm::Charge::Exact)
    }

    pub fn new_with_charge(dialect: &str, charge: lash_kernel_vm::Charge) -> Result<Self, String> {
        let dialect = match dialect {
            "typescript" => lash_ext_helper_compiler::library::Dialect::TypeScript,
            "python" => lash_ext_helper_compiler::library::Dialect::Python,
            _ => return Err(format!("unknown dialect {dialect}")),
        };
        let built = lash_ext_helper_compiler::library::Library::of(dialect)?;
        let mut registry = built.registry;
        let helpers = built.helpers;
        let library = built.names;
        // The runner installs each dialect's operation natives, as a worker
        // does (FIG-6440).
        match dialect {
            lash_ext_helper_compiler::library::Dialect::TypeScript => {
                let functions: Vec<_> = registry.iter().map(|(id, _)| *id).collect();
                lash_ext_date_ecma::implement(&mut registry, functions.iter().copied())
                    .map_err(|e| e.to_string())?;
                lash_ext_url_whatwg::implement(&mut registry, functions)
                    .map_err(|e| e.to_string())?;
                lash_ext_regex_ecma::implement(
                    &mut registry,
                    &Arc::new(lash_ext_regex_ecma::Engine::new(4)),
                )
                .map_err(|e| e.to_string())?;
                lash_ext_ts_core::implement(&mut registry, helpers).map_err(|e| e.to_string())?
            }
            _ => lash_ext_py_core::implement(&mut registry, helpers).map_err(|e| e.to_string())?,
        }
        // Index the library's authored-name projection once for the corpus.
        // Its ordered names retain the same first match as Library::imported.
        let mut helper_names = BTreeMap::new();
        for (name, _) in library.iter() {
            let name = lash_vocabulary::FunctionName::new(name).map_err(|e| e.to_string())?;
            helper_names
                .entry(lash_kernel_dialect::authoring::helper_name(&name))
                .or_insert_with(|| name.to_string());
        }
        Ok(Self {
            library,
            helper_names,
            charge,
            prepared: PreparedLibrary::with_charge(Arc::new(registry), charge),
        })
    }

    pub fn run(&self, case: &Case) -> Result<Observed, String> {
        match self.lower(case)? {
            Lowered::Program(program) => self.execute(case, &program),
            Lowered::Refused(observed) => Ok(observed),
        }
    }

    /// Lowers and admits `case`, or what a refusal to lower it observes.
    pub fn lower(&self, case: &Case) -> Result<Lowered, String> {
        let effects = BTreeMap::from([(
            EffectName::new("host.observe").map_err(|e| e.to_string())?,
            Signature {
                params: vec![Param {
                    name: Name::new("value"),
                    ty: Type::Any,
                    optional: false,
                }],
                result: Type::Any,
            },
        )]);
        let env_namespaces =
            crate::linked::namespaces(&effects, &Default::default()).map_err(|e| e.to_string())?;
        let env = Environment::new(&self.library, &env_namespaces);
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
                    helpers: BTreeSet::new(),
                    charged: None,
                }));
            }
        };

        let admission = lash_kernel_check::Environment::new(&self.library);
        lash_kernel_check::admit(&lowered.document, &admission).map_err(|e| format!("{e:?}"))?;
        Ok(Lowered::Program(crate::linked::program(
            Arc::clone(lowered.document.shared()),
            self.prepared.clone(),
        )))
    }

    /// Runs one admitted program through the embedding API.
    pub fn execute(&self, case: &Case, program: &Program) -> Result<Observed, String> {
        let bounds = Bounds {
            memory_meter: lash_kernel_vm::MemoryMeter::On,
            charge: self.charge,
            charge_limit: 50_000_000,
            memory: 64 << 20,
            call_depth: 200,
            live_tasks: 100,
            requests_per_park: 100,
            join_members: 100,
        };
        let mut machine = crate::pages::start(
            program.clone(),
            bounds,
            CellStart {
                target: Target::Main,
                args: vec![],
                session: None,
                owner: crate::pages::OWNER,
                modes: modes(program),
                host: Vec::new(),
            },
        )
        .map_err(|e| e.to_string())?;
        let helpers = program
            .document
            .imports
            .values()
            // Comparisons classify the authored helper identity, not its
            // encoded package member. Use the library's own name projection.
            .map(|import| {
                self.helper_names
                    .get(&import.name)
                    .cloned()
                    .unwrap_or_else(|| import.name.to_string())
            })
            .collect();
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
                    for request in park.requests {
                        let Request::Effect(effect) = request;
                        let operation = crate::linked::operation(&effect.use_);
                        if operation.as_str().ends_with(".sleep") {
                            machine
                                .deliver(effect.wait, Outcome::Completed(Datum::Null))
                                .map_err(|e| e.to_string())?;
                            continue;
                        }
                        if operation.as_str() != "host.observe" {
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
                Step::Ended(End::Error(RunError::Uncaught {
                    error: Datum::Error(error),
                    ..
                })) => {
                    break json!({"type":"throw", "value":error_class(&error.kind)});
                }
                Step::Ended(End::Error(RunError::Uncaught { error: value, .. })) => {
                    break json!({"type":"throw_value", "value":tag(&value)});
                }
                Step::Ended(other) => return Err(format!("unexpected end: {other:?}")),
            }
        };
        Ok(Observed {
            result,
            helpers,
            charged: machine.meters().charged,
        })
    }
}

/// Bind the fixture's ordinary dialect reads by their exact linked uses.
pub fn modes(program: &Program) -> lash_kernel_vm::EffectModes {
    program
        .instances
        .iter()
        .flat_map(|(link, instance)| {
            program.documents[&instance.document]
                .imports
                .iter()
                .filter_map(move |(slot, import)| {
                    (lash_kernel_dialect::authoring::service_requirement(&import.name).is_some()
                        && import.name.member.as_str() != "sleep")
                        .then_some((
                            lash_vocabulary::OperationUse {
                                link: *link,
                                slot: *slot,
                            },
                            lash_vocabulary::OperationMode::Read,
                        ))
                })
        })
        .collect()
}

struct ScriptHost {
    bits: u64,
    stride: u64,
    nanoseconds: i64,
}
impl Host for ScriptHost {
    fn perform(
        &mut self,
        use_: &lash_vocabulary::OperationUse,
        _: &[Datum],
        _: &lash_vocabulary::sites::PositionLabel,
        _: &lash_vocabulary::sites::Site,
    ) -> Result<Result<Datum, ErrorDatum>, lash_kernel_vm::HostFault> {
        let name = crate::linked::operation(use_);
        Ok(Ok(if name.as_str().ends_with(".clock") {
            Datum::Timestamp(Timestamp {
                nanoseconds: Integer::from(self.nanoseconds),
            })
        } else if name.as_str().ends_with(".random") {
            let bits = self.bits;
            self.bits = self.bits.wrapping_add(self.stride);
            Datum::Bytes(lash_kernel_doc::Bytes::new(bits.to_be_bytes().as_slice()))
        } else {
            return Err(lash_kernel_vm::HostFault::new(format!(
                "unexpected read {name}"
            )));
        }))
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
