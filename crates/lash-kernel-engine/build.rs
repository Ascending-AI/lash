//! Projects the source owners' native membership to static constructor tables.
use std::fmt::Write as _;

#[expect(
    clippy::disallowed_methods,
    reason = "the build writes its constructor tables to OUT_DIR"
)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut source = String::from(
        "static NATIVE_TABLES: &[&[(lash_kernel_value::names::FunctionId, lash_kernel_doc::NativeCtor)]] = &[\n",
    );
    let archive = lash_kernel_releases::helper_archive()?;
    let definitions = archive.validate()?;
    let mut natives = std::collections::BTreeMap::new();
    for release in &archive.releases {
        for (id, identity) in &release.natives {
            if lash_kernel_releases::package(&identity.package).is_some() {
                let callees = definitions[id]
                    .body()
                    .map(|body| {
                        body.functions
                            .iter()
                            .map(|(id, name)| (*id, name.to_string()))
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                natives.insert(*id, (identity.clone(), callees));
            }
        }
    }
    for package in lash_kernel_releases::PACKAGES {
        for (id, name, version) in package.natives {
            natives.insert(
                *id,
                (
                    lash_kernel_releases::archive::NativeIdentity {
                        package: package.path.to_owned(),
                        name: (*name).to_owned(),
                        native_version: *version,
                    },
                    package
                        .native_callees(id)?
                        .into_iter()
                        .map(|(id, name)| (id, name.to_owned()))
                        .collect(),
                ),
            );
        }
    }
    source.push_str("&[\n");
    for (function, (identity, dependencies)) in natives {
        if identity.package != "kernel" {
            let feature = if identity.package == "python" {
                "dialect-python"
            } else {
                "dialect-typescript"
            };
            writeln!(source, "#[cfg(feature = {feature:?})]")?;
        }
        let mut callees = String::from("&[");
        for (callee, label) in dependencies {
            write!(
                callees,
                "(lash_kernel_value::names::FunctionId::from_bytes({:?}), {:?}),",
                callee.as_bytes(),
                label
            )?;
        }
        callees.push(']');
        writeln!(
            source,
            "(lash_kernel_value::names::FunctionId::from_bytes({:?}), |registry| crate::native::native({:?}, {:?}, {}, lash_kernel_value::names::FunctionId::from_bytes({:?}), {callees}, registry)),",
            function.as_bytes(),
            identity.package,
            identity.name,
            identity.native_version,
            function.as_bytes()
        )?;
    }
    source.push_str("],\n");
    source.push_str("];\npub(crate) fn native_tables() -> &'static [&'static [(lash_kernel_value::names::FunctionId, lash_kernel_doc::NativeCtor)]] { NATIVE_TABLES }\n");
    let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").ok_or("OUT_DIR is unset")?);
    std::fs::write(out.join("native_tables.rs"), source)?;
    println!("cargo:rerun-if-changed=build.rs");
    Ok(())
}
