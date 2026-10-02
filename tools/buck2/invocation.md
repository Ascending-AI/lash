# Remote concurrency and daemon ownership

Use `scripts/hermetic-build.sh` or Kiln for managed builds. `--jobs` sets the
stock Buck2 daemon's remote execution semaphore. The default is 32, locally
and in CI. Test runner concurrency uses the same requested limit. Each fork has
its own semaphore, so callers running independent forks must coordinate their
combined remote load.

The pinned official client reads remote endpoints, TLS configuration and the
execution limit from disk when its daemon starts. A `-c` override does not alter
that daemon's remote execution client. The driver therefore writes the limit
into the ignored `.buckconfig.local`, preserving the generated connection
configuration. It hashes both configuration files and the referenced TLS
certificate files to detect a required restart. Receipts contain the combined
hash and limit, never certificate contents.

Commands with the same configuration share an invocation lease and can overlap.
A changed limit or connection identity waits for existing managed commands to
finish. The driver then checks `buck2 status`, refuses a transition if another
command is active, and restarts only that fork's requested isolation. `clean`
also requires exclusive admission. The normal isolation remains `kiln` so
changing concurrency does not change artifact paths or cache keys.

Buck2's file watcher follows directory symlinks when its daemon starts. A link
in the project root that leads back into the project, such as Bazel's
`bazel-<fork>` convenience link in a fork adopted from Bazel, gives source
directories a second name under one inotify watch. The daemon then records
their changes under the link, or receives none once the link is removed, and
keeps building the old sources. Admission therefore removes root `bazel-*`
links as a restart transition, before any daemon can start beside them, and
refuses any other root link that leads back into the project.

Stock `status` reports "no buckd running" for connection failures as well as
absence. Before accepting that result, the guard uses `root --kind daemon`,
checks the owned metadata directory, and takes its lifecycle lock without
waiting. A surviving recorded PID, busy lifecycle, malformed metadata or
unknown process state rejects admission without changing config or receipts.
Config, certificate, receipt and daemon metadata files are opened nonblocking
and checked as regular files before reading, so a FIFO cannot hold admission.

A guardian retains the lease until its Buck client exits, even if the driver
is killed. The guardian closes the lease descriptors in the Buck child: the
stock daemon can otherwise inherit them and retain the lock indefinitely.
Interrupt and termination signals are forwarded to the owned client. The guard
does not kill other forks or cancel their actions.

Direct Buck2 CLI invocations bypass this lease. Do not launch them concurrently
with managed operations that may change daemon configuration. The activity
check detects existing unmanaged commands but cannot atomically exclude a
direct CLI command started between the check and restart. Fork refreshes should
also wait until managed commands finish; admission re-reads refreshed files
after waiting and will not restore an earlier credential snapshot.

Source evidence for official commit
`6507dd157a6f81a810c48583edf1758dd0c337c5`:

- `app/buck2_server/src/daemon/state.rs`: daemon initialization calls
  `BuckConfigBasedCells::parse_with_config_args(&fs, &[])` and constructs
  `RemoteExecutionStaticMetadata` from that root configuration.
- `app/buck2_re_configuration/src/lib.rs`: the OSS `exec_semaphore_size` reads
  `execution_concurrency_limit` from that static metadata.
- `app/buck2_execute/src/re/client.rs`: client creation constructs the semaphore;
  remote execution acquires it before submitting work.
- `app/buck2_client/src/commands/status.rs`: `ExistingOnly` status reports
  project root, isolation and active commands without starting a daemon.
- `app/buck2_daemon/src/daemonize.rs`: daemonization forks and redirects standard
  streams without closing arbitrary inherited descriptors.
