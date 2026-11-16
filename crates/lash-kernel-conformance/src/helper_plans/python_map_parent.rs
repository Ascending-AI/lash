//! K-LIB-006 / K-CHG-007: parent observations for resident dictionary reads.
use super::*;
use lash_kernel_vm::KernelMachine;
use serde::Serialize;

// Recorded on f952da8aae2049661739aafe8846a7f1b742f792 before the VM cut.
// Each digest commits to results, detailed faults, charge events, every slice
// and cold task state, with the full raw observations retained in its receipt.
const PARENT: &[(&str, &str, &str)] = &[
    (
        "records",
        "warm-cold",
        "ccac6a432d2e55fecbe6521a878f85e644b88110715dbed74690f6fb026269b8",
    ),
    (
        "records",
        "events",
        "6511bfa592c4295d212555180f41770ba3acef10c9ef9385a56c86ed4b451774",
    ),
    (
        "records",
        "charge-prefixes",
        "d97c1e570b01b4b4e7bf2acad957ea3cd845ed47e4cffc332f661b5d963db909",
    ),
    (
        "records",
        "depth",
        "ab8725841cc8f943a952e471221278c3175985fd3eaad61fdba90d32463ff2ea",
    ),
    (
        "records",
        "memory",
        "a9d32fde7fc89e86f70af385a5379766745e97d44b0c7cfec0344cd95eec64ab",
    ),
    (
        "alias",
        "warm-cold",
        "0a7bf817eae67f10f80a5599494af0b20add4ea5a53dd02c70cf62a26c09fcc3",
    ),
    (
        "alias",
        "events",
        "e2b4bd981209899202bc73e5f55bfea789191bff6c19a0c1e8580a36a0aadaa2",
    ),
    (
        "alias",
        "charge-prefixes",
        "82cc3da9e0b5275a3903df9ef7817c8993afa1f802479f3fb632a68f2795fb8a",
    ),
    (
        "alias",
        "depth",
        "02624337d7ef3cad99ef286284add43b28ab1433654b4cde7df558c1c735078b",
    ),
    (
        "alias",
        "memory",
        "a47d6d6e14aac726029706789f34325ff04b217a9e2047f6a5eba98cfc6eb8fd",
    ),
    (
        "missing",
        "warm-cold",
        "7e4e58ecbcca4009d3373322447857aa94a52c8d425d518432649a48b118e1d5",
    ),
    (
        "missing",
        "events",
        "6f89e1e9d5dd545516288f799ec14a57b127d134b476c97dda055479da7f254e",
    ),
    (
        "missing",
        "charge-prefixes",
        "6d18d53bb6540d12214a8e8336d52d7ccbb53c83c02965ecc92fe27c6a04597c",
    ),
    (
        "missing",
        "depth",
        "91309387799b9065a0b778e0175a285c102aaeb72893098a6768f809610fe7dc",
    ),
    (
        "missing",
        "memory",
        "14f1a28bb2449f2a030f08b0b35a36a89a5badc9a76b497cafa6da21da6045e4",
    ),
    (
        "long-key",
        "warm-cold",
        "c18c69d71e5affb13daaef143b7d7422083f4eed16ec29f12986b977928a796f",
    ),
    (
        "long-key",
        "events",
        "28712953faf90d0956304c977576a18cd7a1d51943815048e285ec0c1b13ff74",
    ),
    (
        "long-key",
        "charge-prefixes",
        "9b410106ab3e4a57e0ea45eb1c91d02d76b0da659a7ff24da5ed8b48b3809375",
    ),
    (
        "long-key",
        "depth",
        "8106b72ccfdfbf05188340f6ad03c3c1d79879757f955d74c1c6ba3a3443bb9d",
    ),
    (
        "long-key",
        "memory",
        "0ff4509e956c2e526c71e8f6c22fde254fdd84bef80d9c18a19eb32143251e3e",
    ),
    (
        "tuple-key",
        "warm-cold",
        "f0872dbe5dd47e11c1d2480f8742d4535f9e618b47e1cd456d3094d7ab2aa0fa",
    ),
    (
        "tuple-key",
        "events",
        "a2a6745e15766819c3ae3ce1dd51a24f9a2745342bf305fbbffea65b393dea03",
    ),
    (
        "tuple-key",
        "charge-prefixes",
        "4fa257d16a392012fbc332056182393fc03924a0357bf6ff60abd5693e0d2ebb",
    ),
    (
        "tuple-key",
        "depth",
        "4586deefe77a86a6c2ef2020aa702d63dabfd6f8db909079e84422efec935edb",
    ),
    (
        "tuple-key",
        "memory",
        "4a41ddc867788152b3c14d0c5245ded6ba3bd1ac51b4ced03a61a891fe17c6ab",
    ),
    (
        "list-index",
        "warm-cold",
        "d09df0719a77dd9214b1d697e93c5c740cf253184d1ff2e5664689369b569763",
    ),
    (
        "list-index",
        "events",
        "2567418c8d881c36f9446092be9a5b649089279eefc1a04886847d534ddad0a6",
    ),
    (
        "list-index",
        "charge-prefixes",
        "1450e1e20cf892f4eae25107430df1810024700b4728c6dee3ed0efbfdf1796b",
    ),
    (
        "list-index",
        "depth",
        "78556698b32ac5d45c601b143ff5afa9fe8c3226e4807b7e71b9eb16cd6a857e",
    ),
    (
        "list-index",
        "memory",
        "0b01295e1e62acbf879a983693fdb0db6a08ab9ed50605fe5fc38e9da72a5633",
    ),
    (
        "keys",
        "warm-cold",
        "ee54c6c83a67522410ed48fb99ad249ac513f8a81d78dd8c00c3fc57a0181ba2",
    ),
    (
        "keys",
        "events",
        "0704727e60020c9f7f381e9b95dc419da91b4da5960535b4b51550a162a89ba4",
    ),
    (
        "keys",
        "charge-prefixes",
        "8eea300b4f4ebcc2db4b5bd1cb822e7f615f6567ed80006b02885cdabb6fd057",
    ),
    (
        "keys",
        "depth",
        "1676f1c0f9f8388d9d5582d610dbfeedb37bb7d24138cf3823433bf8031e4c29",
    ),
    (
        "keys",
        "memory",
        "843b3d3a12dfe90cc2f544d345e149fe5dc3dacf4ad5a9ad3b534299483e2733",
    ),
];

const SOURCES: &[(&str, &str)] = &[
    (
        "records",
        "rows = [{\"active\": i % 2 == 0, \"value\": i} for i in range(4)]\nselected = [r for r in rows if r[\"active\"]]\nmapped = [r[\"value\"] * 2 for r in selected]\nprint(sum(mapped))",
    ),
    (
        "alias",
        "r = {\"x\": [7]}\na = r\nprint(r[\"x\"])\nr[\"x\"] = [8]\nprint(a[\"x\"])",
    ),
    ("missing", "r = {\"x\": 7}\nprint(r[\"missing\"])"),
    (
        "long-key",
        "r = {\"a key longer than inline text storage\": 9}\nprint(r[\"a key longer than inline text storage\"])",
    ),
    ("tuple-key", "r = {(1, 2): 9}\nprint(r[(1, 2)])"),
    ("list-index", "xs = [7, 8]\nprint(xs[-1])\nprint(xs[0])"),
    (
        "keys",
        "items = [{\"key\": 3}, {\"key\": 1}, {\"key\": 2}]\nitems.sort(key=lambda item: item[\"key\"])\nprint(items)",
    ),
];

#[derive(Debug, Serialize)]
struct Observed {
    end: String,
    prints: Vec<Datum>,
    stops: Vec<(String, u64)>,
    states: Vec<String>,
    events: Vec<String>,
    memory: u64,
}

fn bounds() -> Bounds {
    Bounds {
        memory_meter: lash_kernel_vm::MemoryMeter::On,
        charge: lash_kernel_vm::Charge::Exact,
        charge_limit: 50_000_000,
        memory: 64 << 20,
        call_depth: 200,
        live_tasks: 100,
        requests_per_park: 100,
        join_members: 100,
    }
}

fn observe(program: &Program, bounds: Bounds, slice: u64, cold: bool, record: bool) -> Observed {
    let source = crate::pages::Source::default();
    let machine = KernelMachine::start(
        program.clone(),
        bounds,
        CellStart {
            owner: crate::pages::OWNER,
            target: Target::Main,
            args: Vec::new(),
            session: None,
            modes: Default::default(),
            host: Vec::new(),
        },
        Box::new(source.clone()),
    );
    let mut machine = match machine {
        Ok(machine) => machine,
        Err(error) => {
            return Observed {
                end: error.to_string(),
                prints: Vec::new(),
                stops: Vec::new(),
                states: Vec::new(),
                events: Vec::new(),
                memory: 0,
            };
        }
    };
    let tape = lash_kernel_vm::ChargeTape::new();
    if record {
        machine.record_charges(tape.clone());
    }
    let mut world = World::default();
    let mut stops = Vec::new();
    let mut states = Vec::new();
    loop {
        let step = machine.run(&mut world, slice).unwrap();
        stops.push((
            match &step {
                Step::Slice => "slice",
                Step::Parked(_) => "parked",
                Step::Ended(_) => "ended",
            }
            .into(),
            machine.meters().charged.unwrap(),
        ));
        if let Step::Ended(end) = step {
            let events = tape
                .steps()
                .into_iter()
                .flat_map(|weight| match weight {
                    lash_kernel_vm::Weight::Units(n) => vec!["unit".to_owned(); n as usize],
                    atomic => vec![format!("{atomic:?}")],
                })
                .collect();
            return Observed {
                end: format!("{end:?}"),
                prints: world.prints,
                stops,
                states,
                events,
                memory: machine.meters().memory,
            };
        }
        assert!(matches!(step, Step::Slice));
        if cold {
            let sealed = machine.checkpoint().unwrap();
            let (run, _) = lash_kernel_state::paged::run::continuation::open(
                Box::new(source.clone()),
                lash_kernel_vm::kernel_internals::sealed_root(&sealed).heap,
            )
            .unwrap();
            states.push(format!("{:?}", run.tasks));
            machine = KernelMachine::open(
                program.clone(),
                bounds,
                &sealed.checkpoint(),
                Box::new(source.clone()),
            )
            .unwrap();
            if record {
                machine.record_charges(tape.clone());
            }
        }
    }
}

#[test]
fn python_dictionary_reads_keep_exact_parent_observations() {
    let runner = runner::Runner::new("python").unwrap();
    for (id, source) in SOURCES {
        let runner::Lowered::Program(program) = runner
            .lower(&runner::Case {
                id: (*id).into(),
                dialect: "python".into(),
                source: (*source).into(),
                bits: 0,
                stride: 0,
                nanoseconds: 0,
            })
            .unwrap()
        else {
            panic!("fixture {id} must lower")
        };
        let before = lash_kernel_vm::testing::resident_accesses();
        let full = observe(&program, bounds(), u64::MAX, false, false);
        let hits = lash_kernel_vm::testing::resident_accesses() - before;
        // Direct typed dictionaries lower to dict_getitem, whose source
        // has a different arm. This cut certifies only the generic getter.
        if matches!(*id, "records" | "keys") {
            assert!(hits > 0, "the resident dictionary path must run: {id}");
        } else {
            assert_eq!(hits, 0, "the unproved operand must keep its helper: {id}");
        }
        let charged = full.stops.last().map_or(0, |stop| stop.1);
        let mut groups: Vec<(&str, Vec<Observed>)> = Vec::new();
        groups.push((
            "warm-cold",
            [1, 2, 7, 31, u64::MAX]
                .into_iter()
                .flat_map(|slice| {
                    [false, true]
                        .into_iter()
                        .map(|cold| observe(&program, bounds(), slice, cold, false))
                        .collect::<Vec<_>>()
                })
                .collect(),
        ));
        groups.push((
            "events",
            [false, true]
                .into_iter()
                .map(|cold| observe(&program, bounds(), 7, cold, true))
                .collect(),
        ));
        groups.push((
            "charge-prefixes",
            [
                0,
                1,
                charged / 2,
                charged.saturating_sub(1),
                charged,
                charged + 1,
            ]
            .into_iter()
            .map(|charge_limit| {
                observe(
                    &program,
                    Bounds {
                        charge_limit,
                        ..bounds()
                    },
                    7,
                    false,
                    false,
                )
            })
            .collect(),
        ));
        groups.push((
            "depth",
            (0..=5)
                .map(|call_depth| {
                    observe(
                        &program,
                        Bounds {
                            call_depth,
                            ..bounds()
                        },
                        7,
                        false,
                        false,
                    )
                })
                .collect(),
        ));
        groups.push((
            "memory",
            std::iter::once(0)
                .chain((full.memory.saturating_sub(256)..=full.memory + 2048).step_by(128))
                .flat_map(|memory| {
                    [7, u64::MAX]
                        .into_iter()
                        .map(|slice| {
                            observe(&program, Bounds { memory, ..bounds() }, slice, false, false)
                        })
                        .collect::<Vec<_>>()
                })
                .collect(),
        ));
        for (group, observations) in groups {
            let bytes = serde_json::to_vec(&observations).unwrap();
            let digest = lash_kernel_doc::heap_values::key_digest(&bytes);
            let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
            let expected = PARENT
                .iter()
                .find(|row| row.0 == *id && row.1 == group)
                .unwrap()
                .2;
            assert_eq!(hex, expected, "{id}; {group}; exact parent f952da8aae");
        }
    }
}
