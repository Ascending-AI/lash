//! One cold execution for the cross-interpreter instrument. No warmup or
//! result-validation run is hidden inside the instruction-counted process.
use super::*;

#[expect(
    clippy::disallowed_methods,
    clippy::expect_used,
    clippy::print_stdout,
    reason = "explicit benchmark arguments, source input and JSON receipt"
)]
pub(super) fn main() -> Result<(), String> {
    let began = Instant::now();
    let allocations = stats_alloc::Region::new(ALLOCATOR);
    let mut args = std::env::args().skip(2);
    let file = args.next().ok_or("embedded needs one .ts or .py file")?;
    if args.next().is_some() || !(file.ends_with(".ts") || file.ends_with(".py")) {
        return Err("embedded needs exactly one .ts or .py file".into());
    }
    if cfg!(debug_assertions) {
        return Err("embedded comparisons require an optimized build".into());
    }
    let charge = match std::env::var("KERNEL_PERF_CHARGE").as_deref() {
        Ok("off") => lash_kernel_vm::Charge::Off,
        Ok("exact") | Err(_) => lash_kernel_vm::Charge::Exact,
        Ok(other) => return Err(format!("unknown charge policy: {other}")),
    };
    let bounds = Bounds {
        charge,
        ..benchmark_bounds()
    };
    let python = file.ends_with(".py");
    let mut source = std::fs::read_to_string(&file).map_err(|error| error.to_string())?;
    // Only the result boundary differs from the CLI engines' print(result).
    source.push_str(if python {
        "\nawait finish(result)\n"
    } else {
        "\nawait finish(result);\n"
    });
    let registry = registry();
    let mut registry = (*registry).clone();
    let mut names = NamedLibrary::from_registry(&registry).expect("unique library names");
    let mut helpers = Vec::new();
    for definition in
        lash_dialect_python::define_helpers(&mut names).map_err(|error| error.to_string())?
    {
        helpers.push(
            registry
                .register(definition, None)
                .map_err(|error| error.to_string())?,
        );
    }
    lash_ext_py_core::implement(&mut registry, helpers).expect("Python natives");
    let registry = Arc::new(registry);
    let library = NamedLibrary::from_registry(&registry).expect("unique library names");
    let effects = effects();
    let controls = BTreeMap::from([(
        EffectName::new("finish").expect("tool name"),
        BTreeSet::from([EffectControl::Finish]),
    )]);
    let environment = Environment {
        library: &library,
        effects: &effects,
        tool_roots: &BTreeSet::new(),
        controls: &controls,
    };
    let registry_ns = began.elapsed().as_nanos();
    let lower = Instant::now();
    let mode = lash_kernel_dialect::Mode::Cell(lash_kernel_dialect::CellEnvironment::first());
    let document = if python {
        lash_dialect_python::lower(&source, &environment, mode)
            .map(|lowered| lowered.document)
            .map_err(|error| format!("{error:?}"))?
    } else {
        lash_dialect_typescript::lower(&source, &environment, mode)
            .map(|lowered| lowered.document)
            .map_err(|error| format!("{error:?}"))?
    };
    let lower_ns = lower.elapsed().as_nanos();
    let compile = Instant::now();
    let program = Program {
        release: pages::RELEASE,
        document: Arc::clone(document.shared()),
        library: PreparedLibrary::with_charge(registry, charge),
    };
    let library_compile_ns = compile.elapsed().as_nanos();
    // Document compilation is lazy inside start, together with cell setup.
    let start_time = Instant::now();
    let mut machine = KernelMachine::start(
        program.clone(),
        bounds,
        CellStart {
            owner: pages::OWNER,
            target: Target::Main,
            args: Vec::new(),
            session: None,
            modes: lash_kernel_vm::EffectModes::recorded(),
            host: Vec::new(),
        },
        Box::new(pages::Source::default()),
    )
    .map_err(|error| error.to_string())?;
    let document_compile_and_start_ns = start_time.elapsed().as_nanos();
    let run_allocations = stats_alloc::Region::new(ALLOCATOR);
    let memory_witness = std::env::var_os("KERNEL_PERF_MEMORY_WITNESS").is_some();
    if memory_witness {
        lash_kernel_vm::perf_witness::enable_work();
    }
    let run = Instant::now();
    let end = try_to_end(&mut machine, &file);
    let run_ns = run.elapsed().as_nanos();
    let memory_work = if memory_witness {
        let work = lash_kernel_vm::perf_witness::take_work().map(kernel_phase::phase_counts);
        lash_kernel_vm::perf_witness::disable_work();
        work
    } else {
        None
    };
    let run_allocations = run_allocations.change();
    let cold_ns = began.elapsed().as_nanos();
    let allocations = allocations.change();
    let (outcome, charged, result) = match end {
        Ok((charged, result)) => ("ok", charged, plain(result)?),
        Err(end) => (
            "error",
            machine.meters().charged,
            json!({"error": format!("{end:?}")}),
        ),
    };
    println!(
        "{}",
        json!({
            "outcome": outcome,
            "memory": machine.meters().memory,
            "memory_work": memory_work,
            "result": result,
            "registry_ns": registry_ns,
            "lower_ns": lower_ns,
            "library_compile_ns": library_compile_ns,
            "document_compile_and_start_ns": document_compile_and_start_ns,
            "run_ns": run_ns,
            "cold_ns": cold_ns,
            "charged": charged,
            "allocations": allocations.allocations,
            "run_allocations": run_allocations.allocations,
            "run_reallocations": run_allocations.reallocations,
            "compiler": lash_dialect_typescript::BUILD_IDENTITY.0,
            "build_profile": lash_dialect_typescript::BUILD_IDENTITY.1,
            "observer": "off",
            "durability": format!("{:?}", bounds.durability).to_lowercase(),
        })
    );
    Ok(())
}

/// The common JSON value domain, independent of Datum's tagged wire format.
fn plain(value: Datum) -> Result<Json, String> {
    match value {
        Datum::Null => Ok(Json::Null),
        Datum::Bool(value) => Ok(Json::Bool(value)),
        Datum::Int(value) => {
            serde_json::from_str(&value.to_string()).map_err(|error| error.to_string())
        }
        Datum::Float(value) => value
            .json_number()
            .map(Json::Number)
            .ok_or("nonfinite result".into()),
        Datum::Text(value) => Ok(Json::String(value)),
        Datum::List(values) | Datum::Tuple(values) => values.into_iter().map(plain).collect(),
        Datum::Record(values) => values
            .into_iter()
            .map(|(key, value)| Ok((key, plain(value)?)))
            .collect(),
        Datum::Map(values) => values
            .into_iter()
            .map(|(key, value)| match key {
                Datum::Text(key) => Ok((key, plain(value)?)),
                other => Err(format!("non-text result key: {other:?}")),
            })
            .collect(),
        other => Err(format!("result outside common JSON domain: {other:?}")),
    }
}
