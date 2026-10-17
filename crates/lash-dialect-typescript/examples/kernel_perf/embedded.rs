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
        library: PreparedLibrary::new(registry),
    };
    let library_compile_ns = compile.elapsed().as_nanos();
    // Document compilation is lazy inside start, together with cell setup.
    let start_time = Instant::now();
    let mut machine = start(&program);
    let document_compile_and_start_ns = start_time.elapsed().as_nanos();
    let run_allocations = stats_alloc::Region::new(ALLOCATOR);
    let run = Instant::now();
    let (charged, result) = to_end(&mut machine, &file);
    let run_ns = run.elapsed().as_nanos();
    let run_allocations = run_allocations.change();
    let cold_ns = began.elapsed().as_nanos();
    let allocations = allocations.change();
    let result = plain(result)?;
    println!(
        "{}",
        json!({
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
