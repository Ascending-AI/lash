# Compiler self-profiles

From a Kiln fork with `env.sh` sourced, run:

```sh
python3 scripts/compiler_self_profile.py \
  //crates/lash-core-execution:lash-core-execution \
  --summarize "$HOME/.cargo/bin/summarize" \
  --out-dir .kiln/compiler-self-profile/execution
```

This builds the chosen crate, downloads its rustc self-profile from NativeLink,
and runs measureme's `summarize summarize` on the `.mm_profdata` output. It prints
the top 15 query/phase labels by **self time**, their inclusive time and counts,
and the decoder's total CPU time. `--top N` changes the displayed row count.
The new output directory holds the full text summary, Buck2 build report and
the build command, decoder path and build wall time in `invocation.json`.
Profile data stays in the declared Buck2 output tree; it is not copied into
the evidence directory. Decoder failures fail the command.

Pass multiple explicit first-party library or binary labels to collect one
profile per crate. Patterns, workspace aggregates, tests, third-party labels,
and labels with an existing subtarget are rejected. Dependencies compile
normally. This command does not change optimization levels or resource budgets.

The underlying opt-in Kiln configuration is:

```sh
kiln build --config=rust-self-profile --materializations=final \
  //crates/lash-core-execution:lash-core-execution \
  --build-report .kiln/compiler-self-profile/build.json
```

The driver selects the pinned prelude's `[profile][rustc_stages][raw]` subtarget.
The prelude already declares a directory output and passes its output artifact
to `-Zself-profile=...`, with `-Zself-profile-events=default,args`. A subsequent
`find_profdata` action exposes the file as `self_profile.mm_profdata`. Both
actions run remotely with the crate's existing execution properties. The
report's output group is `profile|rustc_stages|raw`; final materialization
downloads the declared output. No prelude fork or local compiler fallback is
needed. The normal default/static actions, toolchain flags and third-party
rules retain their existing action keys. `check`, `test` and `clippy` do not
accept the instrumentation configuration.

The profile subtarget compiles PIC library code; for a binary it collects
compiler work rather than a final executable link. It does not instrument
every dependency or represent end-to-end linker time. The table groups compiler
events by label; it is not an application runtime profile. Inclusive times
overlap, and CPU time can exceed wall time when LLVM workers run concurrently.
The profile's own string/event collection also consumes compiler time.

## Decoder compatibility

FIG-5723 decoded a profile from the pinned Rust 1.98.1 compiler with
`summarize` 12.0.3 from measureme commit
`e22edccd0364a3058e8e618f51988d64db0b3e10`. The installed decoder was built
with Rust 1.97.0; the profile producer's version determines format compatibility.
No replacement decoder was needed. To use another installation, pass
`--summarize /path/to/summarize`. See the
[compiler profiling guide](https://rustc-dev-guide.rust-lang.org/profiling.html)
and [measureme](https://github.com/rust-lang/measureme) for the format and tools.

## One cold overhead pair

For a library, compare the unprofiled PIC output with the profile subtarget:

```sh
/usr/bin/time -p kiln build \
  '//crates/lash-core-execution:lash-core-execution[static_pic]' \
  --isolation-dir "self-profile-off-$(date +%s%N)" \
  --no-remote-cache --materializations=final \
  --build-report .kiln/compiler-self-profile/off.json

python3 scripts/compiler_self_profile.py \
  //crates/lash-core-execution:lash-core-execution \
  --cold --summarize "$HOME/.cargo/bin/summarize" \
  --out-dir .kiln/compiler-self-profile/on
```

Create the parent evidence directory before the first command. `--cold` uses
a fresh isolation directory and `--no-remote-cache`, so daemon reuse and cached
outputs are bypassed in both legs. Fresh isolation also changes output paths,
preventing an identical previously cached remote action from being reused.
Each leg includes cold dependency compilation. The first command's `real` and
the second command's `build_wall_seconds` both cover the build invocation,
including materialization; the second excludes decoding.

These builds run on shared NativeLink hosts. Record both wall times, selected
compiler action durations, revision, compiler and decoder versions, and any
resource retry. One pair measures an observation that includes dependency
work, queueing, network and host contention; it does not establish an isolated
profiler overhead percentage or a regression threshold. Use the action log
to verify that both chosen compiler actions executed instead of hitting cache.
