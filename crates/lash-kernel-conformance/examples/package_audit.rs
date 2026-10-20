//! Run the package metering audit; missing size-bound evidence exits nonzero.
//! A fixing lane can rerun one definition by passing its name or identity.
#![allow(clippy::disallowed_methods, reason = "explicit developer audit tool")]

#[path = "../src/package/fixtures.rs"]
mod fixtures;

#[path = "../src/package/loop_bounds.rs"]
mod loop_bounds;

use std::process::ExitCode;

fn main() -> ExitCode {
    let (held, _) = fixtures::installed();
    let selector = std::env::args().nth(1);
    let view = held
        .view(&held.own())
        .unwrap_or_else(|error| panic!("{error}"));
    if let Some(selector) = &selector {
        let exists =
            view.packages().any(|package| {
                package.code().ids().iter().enumerate().any(|(at, id)| {
                    package.code().name(at) == selector || id.to_string() == *selector
                })
            });
        if !exists {
            eprintln!("unknown package definition: {selector}");
            return ExitCode::from(2);
        }
    }
    let bounds = match loop_bounds::reviewed_bounds() {
        Ok(bounds) => bounds,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };
    match lash_kernel_conformance::package::audit_metering(&view, &bounds) {
        Ok(loops) => {
            println!(
                "CONF-METER: {loops} loops have declared continuation charges or reviewed size bounds ({} definitions)",
                bounds.len()
            );
            ExitCode::SUCCESS
        }
        Err(error) => {
            let mut refused = false;
            for finding in error.0.lines().filter(|line| {
                selector.as_ref().is_none_or(|selector| {
                    line.contains(&format!(" {selector} @"))
                        || line.contains(&format!("@{selector} at "))
                })
            }) {
                println!("{finding}");
                refused = true;
            }
            if refused {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            }
        }
    }
}
