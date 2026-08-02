use std::path::PathBuf;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    if let Err(err) = run().await {
        eprintln!("{err}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let Some(command) = args.next() else {
        return Err(usage());
    };
    match command.as_str() {
        "fixed-scripts" => {
            let mut out = None;
            while let Some(arg) = args.next() {
                match arg.as_str() {
                    "--out" => {
                        out = args.next().map(PathBuf::from);
                    }
                    "-h" | "--help" => return Err(usage()),
                    other => return Err(format!("unknown argument `{other}`\n\n{}", usage())),
                }
            }
            let Some(out) = out else {
                return Err(format!("missing --out\n\n{}", usage()));
            };
            let manifest = Box::pin(lash_sim::run_fixed_script_profile(out.as_path()))
                .await
                .map_err(|err| err.to_string())?;
            println!("{}", manifest.manifest_path.display());
            Ok(())
        }
        "run" => {
            let mut out = None;
            let mut profile = "fast-random".to_string();
            let mut seeds = None;
            let mut explicit_seeds = Vec::new();
            let mut max_boundaries = None;
            let mut shard = None;
            let mut mode = lash_sim::runner::SimRunMode::Evidence;
            while let Some(arg) = args.next() {
                match arg.as_str() {
                    "--out" => out = args.next().map(PathBuf::from),
                    "--profile" => {
                        profile = args
                            .next()
                            .ok_or_else(|| format!("missing --profile value\n\n{}", usage()))?;
                    }
                    "--seeds" => {
                        let raw = args
                            .next()
                            .ok_or_else(|| format!("missing --seeds value\n\n{}", usage()))?;
                        seeds = Some(parse_usize("--seeds", &raw)?);
                    }
                    "--seed" => {
                        let raw = args
                            .next()
                            .ok_or_else(|| format!("missing --seed value\n\n{}", usage()))?;
                        explicit_seeds.push(parse_u64("--seed", &raw)?);
                    }
                    "--max-boundaries" => {
                        let raw = args.next().ok_or_else(|| {
                            format!("missing --max-boundaries value\n\n{}", usage())
                        })?;
                        max_boundaries = Some(parse_usize("--max-boundaries", &raw)?);
                    }
                    "--shard" => {
                        let raw = args
                            .next()
                            .ok_or_else(|| format!("missing --shard value\n\n{}", usage()))?;
                        shard = Some(
                            lash_sim::generator::SimShard::parse(&raw)
                                .map_err(|err| format!("{err}\n\n{}", usage()))?,
                        );
                    }
                    "--mode" => {
                        let raw = args
                            .next()
                            .ok_or_else(|| format!("missing --mode value\n\n{}", usage()))?;
                        mode = lash_sim::runner::SimRunMode::parse(&raw)
                            .map_err(|err| format!("{err}\n\n{}", usage()))?;
                    }
                    "-h" | "--help" => return Err(usage()),
                    other => return Err(format!("unknown argument `{other}`\n\n{}", usage())),
                }
            }
            let Some(out) = out else {
                return Err(format!("missing --out\n\n{}", usage()));
            };
            if !explicit_seeds.is_empty() && seeds.is_some() {
                return Err(format!(
                    "--seed and --seeds are mutually exclusive\n\n{}",
                    usage()
                ));
            }
            if !explicit_seeds.is_empty() && shard.is_some() {
                return Err(format!(
                    "--shard partitions a --seeds count and cannot combine with explicit --seed values\n\n{}",
                    usage()
                ));
            }
            if !explicit_seeds.is_empty() && mode == lash_sim::runner::SimRunMode::Search {
                return Err(format!(
                    "--mode search applies to a --seeds count; explicit --seed runs always produce full evidence\n\n{}",
                    usage()
                ));
            }
            let seeds = match seeds {
                Some(seeds) => seeds,
                None => lash_sim::generator::default_seed_count(&profile)
                    .map_err(|err| err.to_string())?,
            };
            let max_boundaries = match max_boundaries {
                Some(max_boundaries) => max_boundaries,
                None => lash_sim::generator::default_max_boundaries(&profile)
                    .map_err(|err| err.to_string())?,
            };
            let report = if explicit_seeds.is_empty() {
                lash_sim::run_generated_sim_profile(
                    out.as_path(),
                    &profile,
                    seeds,
                    max_boundaries,
                    shard.unwrap_or(lash_sim::generator::SimShard::FULL),
                    mode,
                )
                .await
                .map_err(|err| err.to_string())?
            } else {
                lash_sim::run_generated_sim_profile_for_seeds(
                    out.as_path(),
                    &profile,
                    &explicit_seeds,
                    max_boundaries,
                )
                .await
                .map_err(|err| err.to_string())?
            };
            println!("{}", report.summary_path.display());
            Ok(())
        }
        "run-postgres" => {
            let mut out = None;
            let mut profile = "fast-random".to_string();
            let mut explicit_seeds = Vec::new();
            let mut max_boundaries = None;
            while let Some(arg) = args.next() {
                match arg.as_str() {
                    "--out" => out = args.next().map(PathBuf::from),
                    "--profile" => {
                        profile = args
                            .next()
                            .ok_or_else(|| format!("missing --profile value\n\n{}", usage()))?
                    }
                    "--seed" => {
                        let value = args
                            .next()
                            .ok_or_else(|| format!("missing --seed value\n\n{}", usage()))?;
                        explicit_seeds.push(parse_u64("--seed", &value)?);
                    }
                    "--max-boundaries" => {
                        let value = args.next().ok_or_else(|| {
                            format!("missing --max-boundaries value\n\n{}", usage())
                        })?;
                        max_boundaries = Some(parse_usize("--max-boundaries", &value)?);
                    }
                    "-h" | "--help" => return Err(usage()),
                    other => return Err(format!("unknown argument `{other}`\n\n{}", usage())),
                }
            }
            let Some(out) = out else {
                return Err(format!("missing --out\n\n{}", usage()));
            };
            if explicit_seeds.is_empty() {
                return Err(format!(
                    "run-postgres requires at least one --seed\n\n{}",
                    usage()
                ));
            }
            let max_boundaries = match max_boundaries {
                Some(max_boundaries) => max_boundaries,
                None => lash_sim::generator::default_max_boundaries(&profile)
                    .map_err(|err| err.to_string())?,
            };
            let database_url = std::env::var("LASH_POSTGRES_DATABASE_URL")
                .map_err(|_| "missing LASH_POSTGRES_DATABASE_URL".to_string())?;
            let report = lash_sim::run_generated_postgres_replay_for_seeds(
                out.as_path(),
                &profile,
                &explicit_seeds,
                max_boundaries,
                &database_url,
            )
            .await
            .map_err(|err| err.to_string())?;
            println!("{}", report.summary_path.display());
            Ok(())
        }
        "replay" => {
            let trace = args
                .next()
                .map(PathBuf::from)
                .ok_or_else(|| format!("missing trace path\n\n{}", usage()))?;
            let mut out = None;
            while let Some(arg) = args.next() {
                match arg.as_str() {
                    "--out" => out = args.next().map(PathBuf::from),
                    "-h" | "--help" => return Err(usage()),
                    other => return Err(format!("unknown argument `{other}`\n\n{}", usage())),
                }
            }
            let report_path = out.as_ref().map(|out| out.join("replay.json"));
            let report = lash_sim::replay::replay_trace_file(&trace, report_path.as_deref())
                .map_err(|err| err.to_string())?;
            if let Some(report_path) = report_path {
                println!("{}", report_path.display());
            } else {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&report).map_err(|err| err.to_string())?
                );
            }
            Ok(())
        }
        "replay-sqlite" => {
            let trace = args
                .next()
                .map(PathBuf::from)
                .ok_or_else(|| format!("missing trace path\n\n{}", usage()))?;
            let mut out = None;
            while let Some(arg) = args.next() {
                match arg.as_str() {
                    "--out" => out = args.next().map(PathBuf::from),
                    "-h" | "--help" => return Err(usage()),
                    other => return Err(format!("unknown argument `{other}`\n\n{}", usage())),
                }
            }
            let Some(out) = out else {
                return Err(format!("missing --out\n\n{}", usage()));
            };
            std::fs::create_dir_all(&out).map_err(|err| err.to_string())?;
            let db_path = out.join("sqlite-store");
            let report_path = out.join("sqlite-replay.json");
            let _report = lash_sim::sqlite_replay::replay_trace_file_to_sqlite(
                &trace,
                &db_path,
                Some(&report_path),
            )
            .await
            .map_err(|err| err.to_string())?;
            println!("{}", report_path.display());
            Ok(())
        }
        "replay-postgres" => {
            let trace = args
                .next()
                .map(PathBuf::from)
                .ok_or_else(|| format!("missing trace path\n\n{}", usage()))?;
            let mut out = None;
            while let Some(arg) = args.next() {
                match arg.as_str() {
                    "--out" => out = args.next().map(PathBuf::from),
                    "-h" | "--help" => return Err(usage()),
                    other => return Err(format!("unknown argument `{other}`\n\n{}", usage())),
                }
            }
            let Some(out) = out else {
                return Err(format!("missing --out\n\n{}", usage()));
            };
            let database_url = std::env::var("LASH_POSTGRES_DATABASE_URL")
                .map_err(|_| "missing LASH_POSTGRES_DATABASE_URL".to_string())?;
            std::fs::create_dir_all(&out).map_err(|err| err.to_string())?;
            let report_path = out.join("postgres-replay.json");
            let _report = lash_sim::postgres_replay::replay_trace_file_to_postgres(
                &trace,
                &database_url,
                Some(&report_path),
            )
            .await
            .map_err(|err| err.to_string())?;
            println!("{}", report_path.display());
            Ok(())
        }
        "backend-contention" => {
            let mut out = None;
            while let Some(arg) = args.next() {
                match arg.as_str() {
                    "--out" => out = args.next().map(PathBuf::from),
                    "-h" | "--help" => return Err(usage()),
                    other => return Err(format!("unknown argument `{other}`\n\n{}", usage())),
                }
            }
            let Some(out) = out else {
                return Err(format!("missing --out\n\n{}", usage()));
            };
            let report = lash_sim::backend_contention::run_backend_contention_report(out.as_path())
                .await
                .map_err(|err| err.to_string())?;
            println!("{}", report.report_path.display());
            Ok(())
        }
        "sqlite-faults" => {
            let mut out = None;
            let mut seed_count = None;
            let mut explicit_seeds = Vec::new();
            while let Some(arg) = args.next() {
                match arg.as_str() {
                    "--out" => out = args.next().map(PathBuf::from),
                    "--seeds" => {
                        let raw = args
                            .next()
                            .ok_or_else(|| format!("missing --seeds value\n\n{}", usage()))?;
                        seed_count = Some(parse_usize("--seeds", &raw)?);
                    }
                    "--seed" => {
                        let raw = args
                            .next()
                            .ok_or_else(|| format!("missing --seed value\n\n{}", usage()))?;
                        explicit_seeds.push(parse_u64("--seed", &raw)?);
                    }
                    "-h" | "--help" => return Err(usage()),
                    other => return Err(format!("unknown argument `{other}`\n\n{}", usage())),
                }
            }
            let Some(out) = out else {
                return Err(format!("missing --out\n\n{}", usage()));
            };
            if seed_count.is_some() && !explicit_seeds.is_empty() {
                return Err(format!(
                    "--seeds and --seed are mutually exclusive\n\n{}",
                    usage()
                ));
            }
            let seeds = if explicit_seeds.is_empty() {
                lash_sim::sqlite_faults::sqlite_fault_seeds(seed_count.unwrap_or(4))
            } else {
                explicit_seeds
            };
            let report = lash_sim::sqlite_faults::run_sqlite_fault_profile(&out, &seeds)
                .await
                .map_err(|err| err.to_string())?;
            println!("{}", report.report_path.display());
            Ok(())
        }
        "stack-probe" => {
            let Some(kind) = args.next() else {
                return Err(format!("missing stack-probe kind\n\n{}", usage()));
            };
            match kind.as_str() {
                "agent-contract" => {
                    let mut contract = None;
                    let mut stack_bytes = None;
                    while let Some(arg) = args.next() {
                        match arg.as_str() {
                            "--contract" => {
                                contract = Some(args.next().ok_or_else(|| {
                                    format!("missing --contract value\n\n{}", usage())
                                })?);
                            }
                            "--stack-bytes" => {
                                let raw = args.next().ok_or_else(|| {
                                    format!("missing --stack-bytes value\n\n{}", usage())
                                })?;
                                stack_bytes = Some(parse_usize("--stack-bytes", &raw)?);
                            }
                            "-h" | "--help" => return Err(usage()),
                            other => {
                                return Err(format!("unknown argument `{other}`\n\n{}", usage()));
                            }
                        }
                    }
                    let Some(contract) = contract else {
                        return Err(format!("missing --contract\n\n{}", usage()));
                    };
                    let Some(stack_bytes) = stack_bytes else {
                        return Err(format!("missing --stack-bytes\n\n{}", usage()));
                    };
                    lash_sim::runner::run_agent_contract_product_stack_probe(
                        &contract,
                        stack_bytes,
                    )
                    .map_err(|err| err.to_string())?;
                    println!("{contract} passed product stack probe at {stack_bytes} bytes");
                    Ok(())
                }
                other => Err(format!("unknown stack-probe kind `{other}`\n\n{}", usage())),
            }
        }
        "minimize" => {
            let trace = args
                .next()
                .map(PathBuf::from)
                .ok_or_else(|| format!("missing trace path\n\n{}", usage()))?;
            let mut out = None;
            while let Some(arg) = args.next() {
                match arg.as_str() {
                    "--out" => out = args.next().map(PathBuf::from),
                    "-h" | "--help" => return Err(usage()),
                    other => return Err(format!("unknown argument `{other}`\n\n{}", usage())),
                }
            }
            let Some(out) = out else {
                return Err(format!("missing --out\n\n{}", usage()));
            };
            let report = lash_sim::minimize::minimize_trace_or_fixture_file(&trace, out.as_path())
                .await
                .map_err(|err| err.to_string())?;
            let report_path = out.join("minimize.json");
            std::fs::write(
                &report_path,
                serde_json::to_vec_pretty(&report).map_err(|err| err.to_string())?,
            )
            .map_err(|err| err.to_string())?;
            println!("{}", report_path.display());
            Ok(())
        }
        "-h" | "--help" => Err(usage()),
        other => Err(format!("unknown command `{other}`\n\n{}", usage())),
    }
}

fn parse_usize(name: &str, raw: &str) -> Result<usize, String> {
    raw.parse::<usize>()
        .map_err(|err| format!("invalid {name} value `{raw}`: {err}"))
}

fn parse_u64(name: &str, raw: &str) -> Result<u64, String> {
    raw.parse::<u64>()
        .map_err(|err| format!("invalid {name} value `{raw}`: {err}"))
}

fn usage() -> String {
    "Usage:
  lash-sim fixed-scripts --out <artifact-root>
  lash-sim run --out <artifact-root> [--profile fast-random] [--seeds N | --seed U64 ...] [--max-boundaries N] [--shard I/N] [--mode evidence|search]
  lash-sim run-postgres --out <artifact-root> [--profile fast-random] --seed U64 ... [--max-boundaries N]
  lash-sim replay <trace> [--out <artifact-root>]
  lash-sim replay-sqlite <trace> --out <artifact-root>
  lash-sim replay-postgres <trace> --out <artifact-root>
  lash-sim backend-contention --out <artifact-root>
  lash-sim sqlite-faults --out <artifact-root> [--seeds N | --seed U64 ...]
  lash-sim stack-probe agent-contract --contract <semantic-oracle> --stack-bytes <bytes>
  lash-sim minimize <trace> --out <artifact-root>"
        .to_string()
}
