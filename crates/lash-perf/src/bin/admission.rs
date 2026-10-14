//! FIG-6255: counted admission of the spike's figments-16 cell.
//! Run an optimized build on a pool performance core, under sudo for perf_event_open.
#![expect(unsafe_code, reason = "Linux instructions:u counter")]

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, ensure};
use lash_kernel_doc::{EffectName, FunctionCatalog, Name, Param, Signature, Type};

#[repr(C)]
#[derive(Default)]
struct PerfEventAttr {
    kind: u32,
    size: u32,
    config: u64,
    sample_period: u64,
    sample_type: u64,
    read_format: u64,
    flags: u64,
    wakeup_events: u32,
    bp_type: u32,
    config1: u64,
}

struct Counter(i32);

impl Counter {
    fn open() -> Result<Self> {
        let attr = PerfEventAttr {
            size: 64,
            config: 1,                      // PERF_COUNT_HW_INSTRUCTIONS
            flags: 1 | (1 << 5) | (1 << 6), // disabled, exclude kernel/hypervisor
            ..Default::default()
        };
        let fd = unsafe { libc::syscall(libc::SYS_perf_event_open, &attr, 0, -1, -1, 0u64) };
        ensure!(
            fd >= 0,
            "perf_event_open: {}",
            std::io::Error::last_os_error()
        );
        Ok(Self(i32::try_from(fd)?))
    }

    fn measure<T>(&self, operation: impl FnOnce() -> T) -> Result<(T, u64)> {
        unsafe {
            ensure!(libc::ioctl(self.0, 0x2403, 0) == 0, "reset counter");
            ensure!(libc::ioctl(self.0, 0x2400, 0) == 0, "enable counter");
        }
        let result = operation();
        let mut instructions = 0u64;
        unsafe {
            ensure!(libc::ioctl(self.0, 0x2401, 0) == 0, "disable counter");
            ensure!(
                libc::read(self.0, (&raw mut instructions).cast(), 8) == 8,
                "read counter"
            );
        }
        Ok((result, instructions))
    }
}

impl Drop for Counter {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.0);
        }
    }
}

fn main() -> Result<()> {
    let mode = std::env::args()
        .nth(1)
        .context("registered or unregistered")?;
    ensure!(
        matches!(mode.as_str(), "registered" | "unregistered"),
        "unknown catalog mode"
    );
    let counter = Counter::open()?;
    let (standard, assembly) = counter.measure(lash_vm_library::standard_functions)?;
    let standard = standard?;
    let (embedding, boot) = counter
        .measure(|| lash_vm_worker::built_dialects(&lash_vm_client::WorkerTuning::default()))?;
    let embedding = embedding?;
    let workload =
        lash_perf::workload::Workload::parse(include_str!("../../workloads/figments-v1.json"))?;
    let generator = lash_perf::workload::Generator::new(&workload, "docrepr")?;
    let plan = generator.plan(16 % 7, 16)?;
    let response = generator.provider_response(&plan, 1)?;
    let source = response.cell_source.context("figments-16 has a cell")?;
    let mut effects = BTreeMap::new();
    let mut roots = BTreeSet::new();
    // The spike gives each named tool its Any -> Any signature.
    for root in ["tools", "control", "processes"] {
        let needle = format!("{root}.");
        let mut at = 0;
        while let Some(found) = source[at..].find(&needle) {
            let start = at + found + needle.len();
            let end = source[start..]
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .map_or(source.len(), |end| start + end);
            if end > start {
                effects.insert(
                    EffectName::new(format!("{root}.{}", &source[start..end]))?,
                    Signature {
                        params: vec![Param {
                            name: Name::new("x"),
                            ty: Type::Any,
                            optional: false,
                        }],
                        result: Type::Any,
                    },
                );
                roots.insert(Name::new(root));
            }
            at = end.max(start);
        }
    }
    let controls = BTreeMap::from([(
        EffectName::new("control.finish")?,
        BTreeSet::from([lash_kernel_dialect::EffectControl::Finish]),
    )]);
    let bindings = BTreeSet::new();
    let functions = BTreeMap::new();
    let environment = lash_kernel_dialect::Environment {
        library: embedding.library(),
        effects: &effects,
        tool_roots: &roots,
        controls: &controls,
        bindings: &bindings,
        functions: &functions,
    };
    let lowered = embedding
        .lower("typescript", &source, &environment)
        .context("TypeScript is installed")?
        .map_err(|error| anyhow::anyhow!("{error:?}"))?;
    let document = &lowered.document;
    let unregistered: BTreeMap<_, _> = standard
        .iter()
        .map(|(id, registered)| (*id, registered.definition.as_ref().clone()))
        .collect();
    let catalog: &dyn FunctionCatalog = if mode == "registered" {
        standard.as_ref()
    } else {
        &unregistered
    };
    let environment = lash_kernel_check::Environment {
        effects: document.manifest.effects.clone(),
        functions: catalog,
        bindings: BTreeSet::new(),
    };
    let (first, cold) = counter.measure(|| lash_kernel_check::admit(document, &environment))?;
    let first = first?;
    let (repeated_result, repeated) = counter.measure(|| -> Result<()> {
        for _ in 0..30 {
            let admitted = lash_kernel_check::admit(document, &environment)?;
            ensure!(
                admitted.identity == first.identity,
                "same document identity"
            );
            std::hint::black_box(admitted);
        }
        Ok(())
    })?;
    repeated_result?;
    println!(
        "{}",
        serde_json::json!({
            "mode": mode, "document_bytes": document.to_json()?.len(), "reached_helpers": document.manifest.functions.len(),
            "registry_helpers": standard.len(), "assembly_instructions": assembly, "worker_boot_instructions": boot,
            "first_admit_instructions": cold, "admit_instructions": repeated / 30, "admits": 30,
        })
    );
    Ok(())
}
