use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const CHILD_TIMEOUT: Duration = Duration::from_secs(15);

pub(crate) fn assert_exact_test_completes(
    test_name: &str,
    child_env: &str,
    watchdog_description: &str,
) {
    let mut child = Command::new(std::env::current_exe().expect("current test binary"))
        .args(["--exact", test_name])
        .env(child_env, "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|err| panic!("failed to spawn {watchdog_description} child: {err}"));
    let deadline = Instant::now() + CHILD_TIMEOUT;

    loop {
        if let Some(status) = child
            .try_wait()
            .unwrap_or_else(|err| panic!("failed to poll {watchdog_description} child: {err}"))
        {
            let (stdout, stderr) = read_output(&mut child, watchdog_description);
            assert!(
                status.success(),
                "{watchdog_description} child failed with {status}\nstdout:\n{stdout}\nstderr:\n{stderr}"
            );
            assert_ran_exactly_one_test(test_name, &stdout, &stderr);
            return;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap_or_else(|err| {
                panic!("failed to kill hung {watchdog_description} child: {err}")
            });
            let _ = child.wait();
            let (stdout, stderr) = read_output(&mut child, watchdog_description);
            panic!(
                "{watchdog_description} exceeded the fifteen-second watchdog\nstdout:\n{stdout}\nstderr:\n{stderr}"
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn read_output(child: &mut Child, watchdog_description: &str) -> (String, String) {
    let stdout = child
        .stdout
        .take()
        .map(std::io::read_to_string)
        .transpose()
        .unwrap_or_else(|err| panic!("failed to read {watchdog_description} stdout: {err}"))
        .unwrap_or_default();
    let stderr = child
        .stderr
        .take()
        .map(std::io::read_to_string)
        .transpose()
        .unwrap_or_else(|err| panic!("failed to read {watchdog_description} stderr: {err}"))
        .unwrap_or_default();
    (stdout, stderr)
}

fn assert_ran_exactly_one_test(test_name: &str, stdout: &str, stderr: &str) {
    let summaries = stdout
        .lines()
        .filter(|line| line.starts_with("test result:"))
        .collect::<Vec<_>>();
    assert_eq!(
        summaries.len(),
        1,
        "watchdog child for `{test_name}` emitted no unique test summary\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        summaries[0].starts_with("test result: ok. 1 passed; 0 failed;"),
        "watchdog child for `{test_name}` did not run exactly one passing test: {}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        summaries[0]
    );
}
