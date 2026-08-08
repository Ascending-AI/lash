//! Public error projection for model-visible RLM observations.

/// Remove host-local path roots once, before an error enters the durable RLM
/// trajectory. The journal therefore replays the same public text on every
/// worker instead of consulting the recovery process's cwd or home directory.
pub(crate) fn public_error_message(error: &str, roots: &[std::path::PathBuf]) -> String {
    let mut prefixes = roots
        .iter()
        .filter(|root| root.components().count() >= 2)
        .filter_map(|root| root.to_str())
        .map(|root| root.trim_end_matches(['/', '\\']))
        .filter(|root| !root.is_empty())
        .collect::<Vec<_>>();
    prefixes.sort_by_key(|prefix| std::cmp::Reverse(prefix.len()));
    prefixes.dedup();

    prefixes
        .into_iter()
        .fold(error.to_string(), |message, prefix| {
            message
                .replace(&format!("{prefix}/"), "")
                .replace(&format!("{prefix}\\"), "")
        })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::public_error_message;

    #[test]
    fn redacts_only_injected_multi_component_roots_at_path_boundaries() {
        let cases = [
            (vec![PathBuf::from("/")], "/tmp/private", "/tmp/private"),
            (
                vec![PathBuf::from("/app")],
                "/app/private /appliance/public /app",
                "private /appliance/public /app",
            ),
            (
                vec![PathBuf::from("$HOME")],
                "$HOME/private",
                "$HOME/private",
            ),
            (
                vec![PathBuf::from("/home/worker/")],
                "/home/worker/.cache/lash",
                ".cache/lash",
            ),
        ];

        for (roots, message, expected) in cases {
            assert_eq!(public_error_message(message, &roots), expected);
        }
    }
}
