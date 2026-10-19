//! Writes each dialect's compiled plans and manifest where they are checked
//! in. Run from the repository root, or give it as the one argument.

#![allow(clippy::disallowed_methods, reason = "explicit source-data recorder")]

use std::path::PathBuf;
use std::process::ExitCode;

use lash_ext_helper_compiler::library::Dialect;

fn main() -> ExitCode {
    let root = std::env::args_os()
        .nth(1)
        .or_else(|| std::env::var_os("BUILD_WORKSPACE_DIRECTORY"))
        .map_or_else(|| PathBuf::from("."), PathBuf::from);
    for dialect in Dialect::ALL {
        let generated = match lash_ext_helper_compiler::generate(dialect) {
            Ok(generated) => generated,
            Err(error) => {
                eprintln!("{}: {error}", dialect.name());
                return ExitCode::FAILURE;
            }
        };
        let (rust, manifest) = lash_ext_helper_compiler::paths(dialect);
        for (path, text) in [(rust, &generated.rust), (manifest, &generated.manifest)] {
            if let Err(error) = std::fs::write(root.join(path), text) {
                eprintln!("{path}: {error}");
                return ExitCode::FAILURE;
            }
        }
        println!("{}: {} plans", dialect.name(), generated.planned);
    }
    // The plan of the `dataframe` fixture's `collect_rows`, for the same
    // laws (FIG-6916).
    let path = lash_ext_helper_compiler::DATAFRAME_PLANS;
    if let Err(error) = lash_ext_helper_compiler::dataframe_plans()
        .and_then(|rust| std::fs::write(root.join(path), rust).map_err(|error| error.to_string()))
    {
        eprintln!("{path}: {error}");
        return ExitCode::FAILURE;
    }
    // The plans of the kernel's body-perform laws (FIG-6916).
    let path = lash_ext_helper_compiler::BODY_PERFORM_PLANS;
    if let Err(error) = lash_ext_helper_compiler::body_perform_plans(include_str!(
        "../../../lash-kernel-vm/src/laws/continuation/body_performs.kernel"
    ))
    .and_then(|rust| std::fs::write(root.join(path), rust).map_err(|error| error.to_string()))
    {
        eprintln!("{path}: {error}");
        return ExitCode::FAILURE;
    }
    // The same compiler records the synthetic bodies of the kernel laws;
    // no hand-written plan is used to test control flow or captures.
    match control_fixture().and_then(|rust| {
        std::fs::write(
            root.join("crates/lash-kernel-conformance/src/helper_plan_control_generated.rs"),
            rust,
        )
        .map_err(|error| error.to_string())
    }) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("control laws: {error}");
            ExitCode::FAILURE
        }
    }
}

fn control_fixture() -> Result<String, String> {
    let mut library = lash_ext_helper_compiler::library::Library::of(Dialect::Python)?;
    let mut names = lash_kernel_dialect::NamedLibrary::from_registry(&library.registry)
        .map_err(|error| error.to_string())?;
    let definitions = lash_kernel_dialect::define_functions(
        include_str!("../../tests/control.kernel"),
        &mut names,
    )
    .map_err(|error| error.to_string())?;
    library.helpers.clear();
    for definition in definitions {
        library.helpers.push(
            library
                .registry
                .register(definition, None)
                .map_err(|error| error.to_string())?,
        );
    }
    Ok(lash_ext_helper_compiler::generate_library(&library, Dialect::Python)?.rust)
}

/// The kernel control laws execute plans emitted by the current compiler,
/// including finally bodies that the dialect corpus need not contain.
#[test]
fn control_plans_are_what_the_compiler_gives_for_the_current_bodies() {
    assert_eq!(
        control_fixture().expect("the control law bodies compile"),
        include_str!("../../../lash-kernel-conformance/src/helper_plan_control_generated.rs"),
        "run kiln run //crates/lash-ext-helper-compiler:regenerate__bin"
    );
}
