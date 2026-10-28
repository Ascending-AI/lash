//! FIG-6985: the first run of a pinned execution whose 30-function closure
//! a worker lacks, from the definitions the store keeps (`loader`), against
//! the same run on a worker whose registry holds them (`registry`).
//! Each run is counted from the library's assembly to the run's end, on a
//! worker that has assembled nothing yet. Run an optimized build with the
//! mode as the only argument; it needs `perf_event_open` for its own
//! user-space instructions:
//! `kiln build --config optimized //crates/lash-vm-worker:coldload__example` and
//! run it under `perf stat -e instructions:u`.
#![expect(unsafe_code, reason = "Linux instructions:u counter")]

use std::collections::BTreeMap;
use std::sync::Arc;

type Outcome<T> = Result<T, Box<dyn std::error::Error>>;
use lash_kernel_doc::heap::{HeapFault, PageGrant, PageId, PageRef};
use lash_kernel_doc::parse_document;
use lash_kernel_value::names::FunctionId;

use lash_kernel_state::paged::{PageAnswer, PageBody, PageSource};
use lash_kernel_vm::Machine as _;
use lash_vm_client::packages::WorkerLibrary;

/// How many functions the run's closure holds beside lash's own.
const CHAIN: usize = 30;

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
    fn open() -> Outcome<Self> {
        let attr = PerfEventAttr {
            size: 64,
            config: 1,                      // PERF_COUNT_HW_INSTRUCTIONS
            flags: 1 | (1 << 5) | (1 << 6), // disabled, exclude kernel/hypervisor
            ..Default::default()
        };
        let fd = unsafe { libc::syscall(libc::SYS_perf_event_open, &attr, 0, -1, -1, 0u64) };
        if fd < 0 {
            return Err(format!("perf_event_open: {}", std::io::Error::last_os_error()).into());
        }
        Ok(Self(i32::try_from(fd)?))
    }

    /// The instructions `operation` takes, once.
    fn once<T>(&self, operation: impl FnOnce() -> T) -> Outcome<(u64, T)> {
        if unsafe { libc::ioctl(self.0, 0x2403, 0) != 0 || libc::ioctl(self.0, 0x2400, 0) != 0 } {
            return Err("the counter does not start".into());
        }
        let value = std::hint::black_box(operation());
        let mut instructions = 0u64;
        if unsafe {
            libc::ioctl(self.0, 0x2401, 0) != 0
                || libc::read(self.0, (&raw mut instructions).cast(), 8) != 8
        } {
            return Err("the counter does not read".into());
        }
        Ok((instructions, value))
    }
}

impl Drop for Counter {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.0);
        }
    }
}

/// Release `step` of package `w`: `w.f0` .. `w.f{CHAIN - 1}`, each calling
/// the one before it.
fn chain(step: i64) -> Outcome<(lash_kernel_doc::FunctionRegistry, FunctionId)> {
    let mut registry = lash_kernel_releases::helper_release()?.functions.clone();
    let mut top = None;
    for at in 0..CHAIN {
        let body = match top {
            None => format!("let out = [n, {step}] return out"),
            Some(id) => format!("let v = invoke @{id}(n) return v"),
        };
        let uses = top.map_or_else(String::new, |id| format!("use w.f{} = @{id}\n", at - 1));
        let definition = lash_kernel_doc::parse_definition(&format!(
            "{uses}function w.f{at}(n: Int) -> Any\nkernel 1\ncharge 1\nbody {{ {body} }}\n"
        ))?;
        top = Some(registry.register(definition, None)?);
    }
    Ok((registry, top.ok_or("empty chain")?))
}

struct World;

impl lash_kernel_vm::Host for World {
    fn print(&mut self, _: &lash_kernel_doc::Datum) {}
    fn cancel_requested(&mut self) -> bool {
        false
    }
}

/// Pages held in memory, each by its content's digest.
#[derive(Default)]
struct Pages(BTreeMap<PageId, PageAnswer>);

impl PageSource for Pages {
    fn read(&mut self, pages: &[PageRef]) -> Result<Vec<Result<PageAnswer, HeapFault>>, HeapFault> {
        Ok(pages
            .iter()
            .map(|page| {
                self.0
                    .get(&page.id)
                    .cloned()
                    .ok_or(HeapFault::Missing { page: page.id })
            })
            .collect())
    }

    fn seal(&mut self, body: PageBody) -> Result<PageRef, HeapFault> {
        let mut tokens = body.kind.0.to_be_bytes().to_vec();
        tokens.extend_from_slice(&body.tokens);
        let page = PageRef {
            id: PageId(lash_kernel_doc::heap_values::key_digest(&tokens)),
            grant: PageGrant::NONE,
        };
        self.0.insert(
            page.id,
            PageAnswer {
                page,
                kind: body.kind,
                tokens: body.tokens.into(),
                children: body.children,
            },
        );
        Ok(page)
    }
}

const BOUNDS: lash_kernel_vm::Bounds = lash_kernel_vm::Bounds {
    memory_meter: lash_kernel_vm::MemoryMeter::On,
    charge: lash_kernel_vm::Charge::Exact,
    charge_limit: 10_000_000,
    memory: 64 << 20,
    call_depth: 100,
    live_tasks: 100,
    requests_per_park: 100,
    join_members: 100,
};

#[expect(
    clippy::disallowed_methods,
    reason = "a measurement binary takes its mode from its command line"
)]
fn main() -> Outcome<()> {
    let mode = std::env::args().nth(1).ok_or("mode: loader or registry")?;
    let (writer, top) = chain(1)?;
    let (reader, _) = chain(2)?;
    let node = match mode.as_str() {
        "loader" => &reader,
        "registry" => &writer,
        other => return Err(format!("unknown mode `{other}`").into()),
    };
    let roots = lash_kernel_releases::helper_release()?.resolver_roots();
    let image = WorkerLibrary::new(node, roots, &Default::default())?;
    let embedding = lash_vm_worker::Embedder::new()
        .dialect(lash_vm_worker::typescript(
            &lash_vm_client::WorkerTuning::standard(),
        )?)?
        .finish(&image)?;
    let closure = lash_vm_broker::kernel::closure::derive(
        [top],
        &writer,
        &BTreeMap::new(),
        &lash_kernel_doc::BuildHelpers::empty(),
        &Default::default(),
    )?;
    let mut text = String::from("kernel 1\nnumbers float\n");
    for function in closure.body().identities() {
        let definition = writer
            .get(&function)
            .ok_or("a declared function")?
            .definition();
        text.push_str(&format!(
            "definition {}\n",
            serde_json::to_string(&lash_kernel_doc::print_definition(definition))?
        ));
        if definition.has_native() {
            text.push_str(&format!("native @{function}\n"));
        }
    }
    text.push_str(&format!("main {{ let a = invoke @{top}(5) return a }}\n"));
    let document = Arc::new(parse_document(&text)?);
    let stored: Vec<(FunctionId, Vec<u8>)> = closure
        .body()
        .identities()
        .filter(|function| node.get(function).is_none())
        .map(|function| {
            (
                function,
                lash_kernel_value::tokens::encode_body(
                    writer
                        .get(&function)
                        .unwrap_or_else(|| {
                            panic!("the cold-load writer lacks definition {function}")
                        })
                        .definition()
                        .as_ref(),
                ),
            )
        })
        .collect();
    let definitions: Vec<(FunctionId, &[u8])> = stored
        .iter()
        .map(|(function, body)| (*function, body.as_slice()))
        .collect();

    let counter = Counter::open()?;
    let (instructions, result) = counter.once(|| -> Outcome<lash_kernel_doc::Datum> {
        let (library, closure) = lash_vm_worker::core_support::library_for(
            &embedding,
            &document,
            &definitions,
            lash_kernel_value::KernelDataLimits::standard(),
            &lash_kernel_doc::Selection::Fresh,
        )?
        .map_err(|refusal| refusal.to_string())?;
        let link = lash_kernel_value::Link {
            helper_fills: Default::default(),
            document: document.identity(),
            fills: BTreeMap::new(),
        };
        let id = link.identity();
        let program = lash_kernel_vm::Program::prepare(
            id,
            BTreeMap::from([(document.identity(), document.clone())]),
            &BTreeMap::from([(id, link)]),
            PageId(closure.reference().0),
            library,
            lash_kernel_doc::Selection::Fresh,
        )?;
        let start = lash_kernel_vm::CellStart {
            owner: PageId([9; 32]),
            target: lash_kernel_vm::Target::Main,
            args: Vec::new(),
            session: None,
            modes: Default::default(),
            host: Vec::new(),
        };
        let mut machine = <lash_kernel_vm::KernelMachine as lash_kernel_vm::Machine>::start(
            program,
            BOUNDS,
            start,
            Box::<Pages>::default(),
        )?;
        loop {
            match machine.run(&mut World, u64::MAX)? {
                lash_kernel_vm::Step::Ended(lash_kernel_vm::End::Finished(done)) => {
                    return Ok(done.result);
                }
                lash_kernel_vm::Step::Slice => {}
                other => return Err(format!("the run ends otherwise: {other:?}").into()),
            }
        }
    })?;
    let result = result?;
    println!(
        "coldload mode={mode} closure={} stored={} instructions={instructions} result={result:?}",
        closure.body().functions().len(),
        stored.len()
    );
    Ok(())
}
