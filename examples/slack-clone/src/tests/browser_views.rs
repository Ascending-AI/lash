#[test]
fn asynchronous_views_publish_only_for_their_current_owner() {
    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/browser_views.mjs");
    let node = std::env::var_os("LASH_SLACK_TEST_NODE").unwrap_or_else(|| "node".into());
    let output = std::process::Command::new(node)
        .arg("--test")
        .arg("--test-reporter=tap")
        .arg(script)
        .output()
        .expect("Node.js is required for the Slack browser view gate");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    for line in stdout.lines() {
        crate::log_out!("{line}");
    }
    assert!(
        output.status.success(),
        "Slack browser view gate failed\nstdout:\n{stdout}\nstderr:\n{stderr}",
    );
}
