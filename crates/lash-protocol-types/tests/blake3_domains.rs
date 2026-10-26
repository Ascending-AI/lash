#[path = "../src/blake3_domains.rs"]
mod blake3_domains;

#[cfg(test)]
#[allow(clippy::disallowed_methods)] // FIG-2971: test module is a host; ambient fs/env/process access is sanctioned
mod blake3_domain_tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::{Path, PathBuf};

    use crate::blake3_domains::{BLAKE3_DOMAINS, RETIRED_BLAKE3_DOMAINS};

    fn rust_sources_below(root: &Path) -> Vec<PathBuf> {
        fn visit(directory: &Path, sources: &mut Vec<PathBuf>) {
            let mut entries = std::fs::read_dir(directory)
                .unwrap_or_else(|error| panic!("read {}: {error}", directory.display()))
                .map(|entry| entry.expect("read workspace source entry").path())
                .collect::<Vec<_>>();
            entries.sort();
            for path in entries {
                if path.is_dir() {
                    visit(&path, sources);
                } else if path.extension().is_some_and(|extension| extension == "rs") {
                    sources.push(path);
                }
            }
        }

        let mut sources = Vec::new();
        visit(root, &mut sources);
        sources
    }

    fn domain_literals(source: &str) -> BTreeSet<String> {
        let constructors = [
            ("Blake3DomainHasher", "::new("),
            ("blake3::Hasher", "::new_derive_key("),
            ("blake3_domain_hash", "("),
            ("blake3_domain_hash_hex", "("),
            ("blake3_hex", "("),
            ("domain_hash", "("),
            ("hex_digest", "("),
        ];
        let constants = source
            .split("const ")
            .skip(1)
            .filter_map(|declaration| {
                let (declaration, _) = declaration.split_once(';')?;
                let (name, definition) = declaration.split_once(':')?;
                let (kind, value) = definition.split_once('=')?;
                if kind.trim() != "&str" {
                    return None;
                }
                let (domain, _) = value.trim().strip_prefix('"')?.split_once('"')?;
                Some((name.trim(), domain))
            })
            .collect::<BTreeMap<_, _>>();
        let mut domains = BTreeSet::new();
        for (name, suffix) in constructors {
            let needle = format!("{name}{suffix}");
            let mut remaining = source;
            while let Some(offset) = remaining.find(&needle) {
                remaining = &remaining[offset + needle.len()..];
                let argument = remaining.trim_start();
                let domain = if let Some(literal) = argument.strip_prefix('"') {
                    literal.split_once('"').map(|(domain, _)| domain)
                } else {
                    argument
                        .split([',', ')'])
                        .next()
                        .and_then(|name| constants.get(name.trim()).copied())
                };
                if let Some(domain) = domain {
                    domains.insert(domain.to_string());
                }
            }
        }
        domains
    }

    #[test]
    fn blake3_domains_are_unique_and_match_workspace_usage() {
        let registered = BLAKE3_DOMAINS
            .iter()
            .map(|domain| (*domain).to_string())
            .collect::<BTreeSet<_>>();
        assert_eq!(
            registered.len(),
            BLAKE3_DOMAINS.len(),
            "BLAKE3 domains are permanently reserved and must be unique"
        );

        let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let used = ["crates", "examples", "runbooks"]
            .into_iter()
            .flat_map(|root| rust_sources_below(&workspace_root.join(root)))
            .flat_map(|path| {
                let source = std::fs::read_to_string(&path)
                    .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
                domain_literals(&source)
            })
            .collect::<BTreeSet<_>>();
        let retired = RETIRED_BLAKE3_DOMAINS
            .iter()
            .map(|d| (*d).to_owned())
            .collect::<BTreeSet<_>>();
        assert_eq!(retired.len(), RETIRED_BLAKE3_DOMAINS.len());
        assert!(
            retired.is_subset(&registered),
            "retired domains remain registered"
        );
        assert!(
            used.is_disjoint(&retired),
            "retired domains must never be reused"
        );
        let active = registered
            .difference(&retired)
            .cloned()
            .collect::<BTreeSet<_>>();
        assert_eq!(
            used, active,
            "the generated BLAKE3_DOMAINS must exactly match the BLAKE3 domains used in workspace \
             Rust sources: declare each as a `lash*/vN` identity constant and run \
             `python3 scripts/release_baseline.py tables --write`"
        );
    }
}
