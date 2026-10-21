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
    let preparation =
        preparation_probe(std::env::var("KERNEL_PERF_PREPARATION").is_ok_and(|value| value == "1"));
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
    let layout = match std::env::var("KERNEL_PERF_REGIONS").as_deref() {
        Ok("off") => lash_kernel_vm::kernel_internals::Layout(0).without_regions(),
        Ok("on") | Err(_) => lash_kernel_vm::kernel_internals::Layout(0),
        Ok(other) => return Err(format!("unknown regions policy: {other}")),
    };
    let bounds = Bounds {
        charge,
        ..benchmark_bounds()
    };
    let python = file.ends_with(".py");
    let mut source = std::fs::read_to_string(&file).map_err(|error| error.to_string())?;
    // Only the result boundary differs from the CLI engines' print(result).
    source.push_str(if python {
        "\nawait perf.finish(result)\n"
    } else {
        "\nawait perf.finish(result);\n"
    });
    let frame = std::env::var_os("KERNEL_PERF_FRAME")
        .map(|path| {
            let bytes = std::fs::read(path).map_err(|error| error.to_string())?;
            let held = lash_vm_client::packages::WorkerLibrary::from_frame(bytes.into())
                .map_err(|error| error.to_string())?;
            let embedding = lash_vm_worker::built_dialects(&Default::default())
                .and_then(|builder| builder.finish(&held))
                .map_err(|error| error.to_string())?;
            let registry = Arc::clone(embedding.registry().map_err(|error| error.to_string())?);
            let prepared = match charge {
                lash_kernel_vm::Charge::Exact => embedding
                    .prepared_library()
                    .map_err(|error| error.to_string())?
                    .clone(),
                lash_kernel_vm::Charge::Off => PreparedLibrary::over_frame(
                    registry.clone(),
                    charge,
                    held.indexes().map_err(|error| error.to_string())?[1],
                )
                .map_err(|error| error.to_string())?,
            };
            Ok::<_, String>((registry, prepared))
        })
        .transpose()?;
    let (registry, prepared) = match frame {
        Some((registry, prepared)) => (registry, Some(prepared)),
        None => (registry(), None),
    };

    let library = NamedLibrary::resolving(&registry, |id| {
        lash_vm_releases::helper_release()
            .expect("helper release")
            .resolver_roots()
            .contains(id)
    })
    .expect("unique library names");
    let effects = effects();
    let controls = BTreeMap::from([(
        EffectName::new("perf.finish").expect("tool name"),
        BTreeSet::from([EffectControl::Finish]),
    )]);
    let environment_namespaces =
        crate::linked::namespaces(&effects, &controls).expect("the test's effects are operations");
    let environment = Environment::new(&library, &environment_namespaces);
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
    let program = crate::linked::program(
        Arc::clone(document.shared()),
        prepared.unwrap_or_else(|| {
            lash_kernel_vm::kernel_internals::in_charge(
                &lash_kernel_vm::kernel_internals::prepare_with_layout(registry, layout),
                charge,
            )
        }),
    );
    let library_compile_ns = compile.elapsed().as_nanos();
    // Document compilation is lazy inside start, together with cell setup.
    let start_time = Instant::now();
    let mut machine = lash_kernel_vm::kernel_internals::start_with_layout(
        program.clone(),
        bounds,
        CellStart {
            owner: pages::OWNER,
            target: Target::Main,
            args: Vec::new(),
            session: None,
            modes: crate::linked::modes(&program),
            host: Vec::new(),
        },
        layout,
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
            "preparation": *preparation.borrow(),
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
            "regions": if layout == lash_kernel_vm::kernel_internals::Layout(0) { "on" } else { "off" },
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

/// Optional first-use compilation attribution. The regular paired population
/// leaves this off; a diagnostic run records each helper separately.
fn preparation_probe(enabled: bool) -> std::rc::Rc<std::cell::RefCell<Vec<Json>>> {
    use lash_kernel_vm::perf_witness::{Preparation, observe_preparation};
    let samples = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    if !enabled {
        return samples;
    }
    let output = std::rc::Rc::clone(&samples);
    let mut pending = None;
    observe_preparation(Some(Box::new(move |event| match event {
        Preparation::Begin { function, callback } => {
            pending = Some((function, callback, Instant::now(), Instructions::start()));
        }
        Preparation::End => {
            if let Some((function, callback, start, counter)) = pending.take() {
                let count = counter.and_then(Instructions::finish);
                output.borrow_mut().push(json!({
                    "function": function.to_string(), "callback": callback,
                    "instructions": count, "wall_ns": start.elapsed().as_nanos(),
                }));
            }
        }
    })));
    samples
}

/// A per-thread instructions:u counter, owned by the diagnostic process.
#[cfg(target_os = "linux")]
struct Instructions(i32);

#[cfg(not(target_os = "linux"))]
struct Instructions;

#[cfg(not(target_os = "linux"))]
impl Instructions {
    fn start() -> Result<Self, String> {
        Err("instructions:u requires Linux".into())
    }
    fn finish(self) -> Result<u64, String> {
        Err("instructions:u requires Linux".into())
    }
}

#[cfg(target_os = "linux")]
#[expect(
    unsafe_code,
    reason = "the optional instrument reads Linux perf instructions:u"
)]
impl Instructions {
    fn start() -> Result<Self, String> {
        let mut attr = [0u64; 8];
        attr[0] = 64u64 << 32;
        attr[1] = 1;
        attr[5] = 1 | (1 << 5) | (1 << 6);
        let fd = unsafe { libc::syscall(libc::SYS_perf_event_open, attr.as_ptr(), 0, -1, -1, 0) };
        let fd = i32::try_from(fd).map_err(|error| error.to_string())?;
        if fd < 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        let counter = Self(fd);
        if unsafe { libc::ioctl(fd, 0x2400, 0) } != 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        Ok(counter)
    }

    fn finish(self) -> Result<u64, String> {
        let mut count = 0u64;
        if unsafe { libc::ioctl(self.0, 0x2401, 0) } != 0
            || unsafe { libc::read(self.0, (&raw mut count).cast(), 8) } != 8
        {
            return Err(std::io::Error::last_os_error().to_string());
        }
        Ok(count)
    }
}

#[cfg(target_os = "linux")]
#[expect(unsafe_code, reason = "the measurement owns its perf descriptor")]
impl Drop for Instructions {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.0);
        }
    }
}

/// Produces exactly the parent frame used by production pool installation.
#[expect(clippy::disallowed_methods, reason = "explicit benchmark frame output")]
pub(super) fn write_frame() -> Result<(), String> {
    let mut args = std::env::args().skip(2);
    let path = args.next().ok_or("write-frame needs one output path")?;
    if args.next().is_some() {
        return Err("write-frame needs exactly one output path".into());
    }
    let release = lash_vm_releases::helper_release().map_err(|error| error.to_string())?;
    let held =
        lash_vm_client::packages::WorkerLibrary::new(&release.functions, release.resolver_roots())
            .map_err(|error| error.to_string())?;
    std::fs::write(path, held.frame()).map_err(|error| error.to_string())
}
