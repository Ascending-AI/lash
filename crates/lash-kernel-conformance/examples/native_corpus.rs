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
    let mut registry = lash_kernel_doc::FunctionRegistry::new();
    lash_kernel_lib::register_numbers(&mut registry)?;
    lash_kernel_lib::register_text_json(&mut registry)?;
    lash_kernel_vm::register_machine_functions(&mut registry)?;
    lash_kernel_lib::register_collections(&mut registry)?;
    if !audit {
        record(&mut recorded, &registry, declared::PYTHON)?;
    }
    if extend {
        recorded.extend_generated(&registry, declared::PYTHON, release)?;
    }
    lash_ext_regex_ecma::register(
        &mut registry,
        &std::sync::Arc::new(lash_ext_regex_ecma::Engine::new(256)),
    )?;
    lash_ext_date_ecma::register(&mut registry)?;
    lash_ext_url_whatwg::register(&mut registry)?;
    if audit {
        for dialect in [declared::PYTHON, declared::TYPESCRIPT] {
            for difference in recorded.audit(&registry, dialect, release)? {
                println!("{}", serde_json::to_string(&difference)?);
            }
        }
        return Ok(());
    }
    record(&mut recorded, &registry, declared::TYPESCRIPT)?;
    if extend {
        recorded.extend_generated(&registry, declared::TYPESCRIPT, release)?;
    }
    recorded.write(
        &std::path::PathBuf::from(root).join("crates/lash-vm-releases/src/build/corpus_data"),
    )?;
    Ok(())
}
