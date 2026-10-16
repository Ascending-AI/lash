//! Explicitly records new native identities without building their release.
//!
//! Run through kiln, passing the checkout root and an optional mode. Every
//! existing answer is certified before source is written; this command cannot
//! re-record an incompatible implementation under a pinned identity.
//! `--replace-unsealed-baseline` intentionally drops superseded identities only
//! from the unsealed release the tree builds; surviving answers stay pinned.

#![allow(clippy::disallowed_methods, reason = "explicit source-data recorder")]

#[path = "../../lash-vm-releases/src/build/corpus.rs"]
mod corpus;
#[expect(
    dead_code,
    reason = "the recorder uses the current release and dialect names; retirement declarations belong to the release build"
)]
#[path = "../../lash-vm-releases/src/declared.rs"]
mod declared;
#[path = "../../lash-vm-releases/src/build/inputs.rs"]
mod inputs;
#[path = "../../lash-vm-releases/src/build/probe.rs"]
mod probe;
#[path = "../../lash-vm-releases/src/build/record.rs"]
mod record;
#[path = "../../lash-vm-releases/src/sealed.rs"]
mod sealed;

/// The kernel library and the machine's own functions, as every build
/// registers them before a dialect adds its helpers.
fn kernel() -> Result<lash_kernel_doc::FunctionRegistry, Box<dyn std::error::Error>> {
    let mut registry = lash_kernel_doc::FunctionRegistry::new();
    lash_kernel_lib::register_numbers(&mut registry)?;
    lash_kernel_lib::register_text_json(&mut registry)?;
    lash_kernel_vm::register_machine_functions(&mut registry)?;
    lash_kernel_lib::register_collections(&mut registry)?;
    Ok(registry)
}

type Failed = Box<dyn std::error::Error>;

/// A dialect's helper definitions, defined against the kernel library.
type Define = fn(
    &mut lash_kernel_dialect::NamedLibrary,
) -> Result<Vec<lash_kernel_doc::FunctionDefinition>, Failed>;

/// Adds a dialect's operation natives to its registered helpers.
type Implement = fn(
    &mut lash_kernel_doc::FunctionRegistry,
    Vec<lash_kernel_doc::FunctionId>,
) -> Result<(), Failed>;

/// Registers the helpers `define` defines against `registry`, as the
/// release build does, and lets `implement` add their operations' natives.
fn helpers(
    registry: &mut lash_kernel_doc::FunctionRegistry,
    define: Define,
    implement: Implement,
) -> Result<(), Failed> {
    let mut library = lash_kernel_dialect::NamedLibrary::from_registry(registry)?;
    let validated = registry.validate_functions(define(&mut library)?)?;
    implement(
        registry,
        validated.iter().map(|(function, _)| *function).collect(),
    )
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = std::env::args_os()
        .nth(1)
        .ok_or("usage: native_corpus <checkout root>")?;
    let mode = std::env::args_os().nth(2);
    let extend = mode.as_ref().is_some_and(|flag| flag == "--extend");
    let audit = mode.as_ref().is_some_and(|flag| flag == "--audit");
    let replace = mode
        .as_ref()
        .is_some_and(|flag| flag == "--replace-unsealed-baseline");
    if mode.is_some() && !extend && !audit && !replace {
        return Err("expected --extend, --audit or --replace-unsealed-baseline".into());
    }
    let mut recorded = corpus::Corpus::recorded()?;
    let release = declared::RETAINED_HELPER_RELEASES
        .last()
        .ok_or("no retained release")?
        .0;
    let record = |recorded: &mut corpus::Corpus,
                  registry: &lash_kernel_doc::FunctionRegistry,
                  dialect: &str| {
        if replace {
            let sealed = sealed::SEALED
                .iter()
                .find(|(name, _)| *name == dialect)
                .map_or(&[][..], |(_, releases)| *releases);
            recorded.replace_unsealed_baseline(registry, dialect, release, sealed)
        } else {
            recorded.record_new(registry, dialect, release)
        }
    };
    // Each dialect's registry is the one its release build certifies.
    let mut python = kernel()?;
    helpers(
        &mut python,
        |library| Ok(lash_dialect_python::define_helpers(library)?),
        |registry, helpers| Ok(lash_ext_py_core::implement(registry, helpers)?),
    )?;
    let mut typescript = kernel()?;
    lash_ext_regex_ecma::register(
        &mut typescript,
        &std::sync::Arc::new(lash_ext_regex_ecma::Engine::new(256)),
    )?;
    lash_ext_date_ecma::register(&mut typescript)?;
    lash_ext_url_whatwg::register(&mut typescript)?;
    helpers(
        &mut typescript,
        |library| Ok(lash_dialect_typescript::define_helpers(library)?),
        |registry, helpers| Ok(lash_ext_ts_core::implement(registry, helpers)?),
    )?;
    let dialects = [
        (declared::PYTHON, &python),
        (declared::TYPESCRIPT, &typescript),
    ];
    if audit {
        for (dialect, registry) in dialects {
            for difference in recorded.audit(registry, dialect, release)? {
                println!("{}", serde_json::to_string(&difference)?);
            }
        }
        return Ok(());
    }
    for (dialect, registry) in dialects {
        record(&mut recorded, registry, dialect)?;
        if extend {
            recorded.extend_generated(registry, dialect, release)?;
        }
    }
    recorded.write(
        &std::path::PathBuf::from(root).join("crates/lash-vm-releases/src/build/corpus_data"),
    )?;
    Ok(())
}
