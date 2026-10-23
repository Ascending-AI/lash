//! Resident reference loops: clocks and JSON transport are outside guest sources.
use super::{Args, Sample, workloads::Workload};
use anyhow::{Result, ensure};
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
    Ok(engines)
}
impl Reference {
    pub fn measure(
        &self,
        args: &Args,
        workload: &Workload,
        trips: [u64; 2],
        dir: &Path,
    ) -> Result<Vec<[Sample; 2]>> {
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
        let output = Command::new(&self.binary)
            .args(&self.flags)
            .arg(&path)
            .output()?;
        ensure!(
            output.status.success(),
            "{} {}: {}",
            self.name,
            workload.name,
            String::from_utf8_lossy(&output.stderr)
        );
        let samples: Vec<[Sample; 2]> = serde_json::from_slice(&output.stdout)?;
        ensure!(
            samples.len() == args.runs,
            "incomplete reference population"
        );
        Ok(samples)
    }
}
