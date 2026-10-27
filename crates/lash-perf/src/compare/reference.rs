//! Resident reference loops: clocks and JSON transport are outside guest sources.
use super::{Args, Sample, workloads::Workload};
use anyhow::{Context, Result, ensure};
use std::path::{Path, PathBuf};
use std::process::Command;

pub(super) struct Reference {
    pub name: String,
    pub binary: PathBuf,
    pub flags: Vec<&'static str>,
    pub python: bool,
    pub identity: String,
}
pub(super) fn engines(args: &Args) -> Result<Vec<Reference>> {
    let mut engines = Vec::new();
    for (i, binary) in args.qjs.iter().enumerate() {
        engines.push(Reference {
            name: format!("qjs-{i}"),
            binary: binary.clone(),
            flags: vec!["--std"],
            python: false,
            identity: super::hash(&std::fs::read(binary)?),
        });
    }
    if let Some(binary) = &args.node {
        for (name, flags) in [("node-jitless", vec!["--jitless"]), ("node-jit", vec![])] {
            engines.push(Reference {
                name: name.into(),
                binary: binary.clone(),
                flags,
                python: false,
                identity: super::hash(&std::fs::read(binary)?),
            });
        }
    }
    if let Some(binary) = &args.python {
        engines.push(Reference {
            name: "cpython".into(),
            binary: binary.clone(),
            flags: vec![],
            python: true,
            identity: super::hash(&std::fs::read(binary)?),
        });
    }
    if let Some(binary) = &args.monty {
        engines.push(Reference {
            name: "monty".into(),
            binary: binary.clone(),
            flags: vec![],
            python: true,
            identity: super::hash(&std::fs::read(binary)?),
        });
    }
    Ok(engines)
}
#[derive(Debug)]
pub(super) struct Unsupported(pub String);
impl std::fmt::Display for Unsupported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for Unsupported {}

impl Reference {
    pub fn measure(
        &self,
        args: &Args,
        workload: &Workload,
        trips: [u64; 2],
        dir: &Path,
    ) -> Result<Vec<[Sample; 2]>> {
        if args.instructions {
            return self.instruction_samples(args, workload, trips, dir);
        }
        let path = dir.join(format!(
            "{}-{}.{}",
            self.name,
            workload.name,
            if self.python { "py" } else { "js" }
        ));
        let source = if self.python {
            format!(
                "{}\nimport json, time\noutput=[]\nfor run in range({total}):\n    pair=[]\n    for n in [{lo},{hi}]:\n        at=time.perf_counter_ns()\n        result=benchmark(n)\n        ns=time.perf_counter_ns()-at\n        pair.append({{\"ns\":ns,\"result\":result}})\n    if run >= {warmups}:\n        output.append(pair)\nprint(json.dumps(output))\n",
                workload.py,
                total = args.runs + args.warmups,
                lo = trips[0],
                hi = trips[1],
                warmups = args.warmups
            )
        } else {
            let clock = if self.name.starts_with("qjs-") {
                "os.now()"
            } else {
                "performance.now()"
            };
            format!(
                "{}\nconst output=[];\nfor(let run=0;run<{total};run++){{const pair=[];for(const n of [{lo},{hi}]){{const at={clock};const result=benchmark(n);const ns=({clock}-at)*1000000;pair.push({{ns:ns,result:result}});}}if(run>={warmups})output.push(pair);}}\nconsole.log(JSON.stringify(output));\n",
                workload.js,
                total = args.runs + args.warmups,
                lo = trips[0],
                hi = trips[1],
                warmups = args.warmups
            )
        };
        std::fs::write(&path, source)?;
        let output = self.invoke(&path, None)?;
        let samples: Vec<[Sample; 2]> = serde_json::from_slice(&output.stdout)?;
        ensure!(
            samples.len() == args.runs,
            "incomplete reference population"
        );
        Ok(samples)
    }
}

impl Reference {
    fn invoke(&self, path: &Path, stat: Option<&Path>) -> Result<std::process::Output> {
        let mut command = if let Some(stat) = stat {
            let mut command = Command::new("perf");
            command
                .args([
                    "stat",
                    "--no-big-num",
                    "--no-scale",
                    "-x",
                    "\t",
                    "-e",
                    "instructions:u",
                    "-o",
                ])
                .arg(stat)
                .arg("--")
                .arg(&self.binary);
            command
        } else {
            Command::new(&self.binary)
        };
        let output = command.args(&self.flags).arg(path).output()?;
        std::fs::write(path.with_extension("stdout"), &output.stdout)?;
        std::fs::write(path.with_extension("stderr"), &output.stderr)?;
        if !output.status.success() {
            let error = String::from_utf8_lossy(&output.stderr).into_owned();
            if self.name == "monty"
                && [
                    "NotImplementedError:",
                    "SyntaxError:",
                    "ImportError:",
                    "ModuleNotFoundError:",
                    "AttributeError:",
                    "NameError:",
                ]
                .iter()
                .any(|kind| error.contains(kind))
            {
                return Err(Unsupported(error).into());
            }
            anyhow::bail!("{} exited {}: {error}", self.name, output.status);
        }
        Ok(output)
    }

    fn instruction_samples(
        &self,
        args: &Args,
        workload: &Workload,
        trips: [u64; 2],
        dir: &Path,
    ) -> Result<Vec<[Sample; 2]>> {
        let mut samples = Vec::new();
        for run in 0..args.runs {
            let mut pair = Vec::new();
            // Alternate low/high order. Every process has the same fixed low-count
            // warmup prefix, which cancels along with parsing/startup/teardown.
            for i in if run % 2 == 0 { [0, 1] } else { [1, 0] } {
                let path = dir.join(format!(
                    "{}-{}-{run}-{i}.{}",
                    self.name,
                    workload.name,
                    if self.python { "py" } else { "js" }
                ));
                let source = if self.python {
                    format!(
                        "{}\nimport json, time\nfor warmup in range({}):\n    benchmark({})\nat=time.perf_counter_ns()\nresult=benchmark({})\nns=time.perf_counter_ns()-at\nprint(json.dumps({{\"ns\":ns,\"result\":result}}))\n",
                        workload.py, args.warmups, trips[0], trips[i]
                    )
                } else {
                    let clock = if self.name.starts_with("qjs-") {
                        "os.now()"
                    } else {
                        "performance.now()"
                    };
                    format!(
                        "{}\nfor(let warmup=0;warmup<{};warmup++) benchmark({});\nconst at={clock};const result=benchmark({});const ns=({clock}-at)*1000000;\nconsole.log(JSON.stringify({{ns:ns,result:result}}));\n",
                        workload.js, args.warmups, trips[0], trips[i]
                    )
                };
                std::fs::write(&path, source)?;
                let stat = path.with_extension("perf.tsv");
                let output = self.invoke(&path, Some(&stat))?;
                let mut sample: Sample = serde_json::from_slice(&output.stdout)?;
                let raw = std::fs::read_to_string(&stat)?;
                let counters = raw
                    .lines()
                    .map(|line| line.split('\t').collect::<Vec<_>>())
                    .filter(|fields| fields.get(2) == Some(&"instructions:u"))
                    .collect::<Vec<_>>();
                ensure!(
                    counters.len() == 1,
                    "missing instructions:u counter: {}",
                    stat.display()
                );
                let fields = &counters[0];
                let count = fields[0]
                    .trim()
                    .parse::<u64>()
                    .context("unavailable instructions:u count")?;
                let running = fields
                    .get(4)
                    .context("missing PMU running percentage")?
                    .trim()
                    .parse::<f64>()?;
                ensure!(
                    count > 0 && (99.0..=100.0).contains(&running),
                    "unqualified instructions:u ({running}% running): {}",
                    stat.display()
                );
                sample.instructions = Some(count);
                pair.push((i, sample));
            }
            pair.sort_by_key(|(i, _)| *i);
            samples.push(
                pair.into_iter()
                    .map(|(_, sample)| sample)
                    .collect::<Vec<_>>()
                    .try_into()
                    .map_err(|_| anyhow::anyhow!("incomplete reference pair"))?,
            );
        }
        Ok(samples)
    }
}
