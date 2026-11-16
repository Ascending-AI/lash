//! K-LIB-006 / K-CHG-007: parent observations for word multiplication.
use super::*;
use lash_kernel_vm::KernelMachine;
use serde::Serialize;

// Exact parent d19b72041263d8af43cbd48a59cc67fb291c20c0, recorded before the cut.
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
        "5f330e8aae88880c1e459cb4219f7aee5342e09d3abd267cde31670ea926d782",
    ),
    (
        "words",
        "warm-cold",
        "14f9d1755b5444bcfe7ed239af43942d4f7c68bddaf0850970f6926f7413c7a1",
    ),
    (
        "words",
        "events",
        "1e167aaba331499885dd5d9ca03430dea44bd9bd717ff6ba6b62ebd73e592c2d",
    ),
    (
        "words",
        "charge-prefixes",
        "5c275b70b1dccd4de6e2cd496e23ea0c50d67eb36bbd3803cb71b4ed7c392c80",
    ),
    (
        "words",
        "depth",
        "951f7883608756ab918fd717d77b7ce61f4f1f1528197de86741bc6138393caf",
    ),
    (
        "words",
        "memory",
        "e56af8d29ef65852d9d9776e83d024c6f9a33d50994189a747c22b35dd36e1f4",
    ),
    (
        "positive-overflow",
        "warm-cold",
        "96ce50d007a5b78782efb480ad07331044a16baa84fed07d024cee00c4c320db",
    ),
    (
        "positive-overflow",
        "events",
        "fd597c4d4a86811cc6a52784119d44f62a1ed930d93f8a015a85081a475d9344",
    ),
    (
        "positive-overflow",
        "charge-prefixes",
        "469487d3d7c4a6e5a8316066188722ac0eb912a8bef61c05d06fbb97d1da7d51",
    ),
    (
        "positive-overflow",
        "depth",
        "bb696a641f285bf4f7751296f080d3a8f10112042e5aa47b243317391fcb8c24",
    ),
    (
        "positive-overflow",
        "memory",
        "a2624264718e33c4e547028ed993401e3c16503824f918bcb4b3039bdba9b8ad",
    ),
    (
        "negative-overflow",
        "warm-cold",
        "4736bc0d3d9179a627300f4ccba59bef3332367233d9840935c8f76bad1bd48d",
    ),
    (
        "negative-overflow",
        "events",
        "dcb985aa3ebf14c499f8e0577c159c3358781925add0403cb058e288fdc632f5",
    ),
    (
        "negative-overflow",
        "charge-prefixes",
        "8dbaec68adb5d08c7f19df10c77ab69dc31afdc1b388807942eb4aaa946c3117",
    ),
    (
        "negative-overflow",
        "depth",
        "04959126483e134f337d3fd624a6a28e604e714fce5b1fbc28b376cad8c0a1bb",
    ),
    (
        "negative-overflow",
        "memory",
        "d1b8966ff8b70b8e889c3c6181383ee89bdefe95378d893027926e7e3a9b7fa8",
    ),
    (
        "big-input",
        "warm-cold",
        "0c6d324a5d5cdc987fa8b69876bf7acc4e2eaf5f15255d28f718c24e5fa0f117",
    ),
    (
        "big-input",
        "events",
        "7250eb4abd8a99af99160765d12583587fcbfdccf5d999087792f9d0dfa7d5f8",
    ),
    (
        "big-input",
        "charge-prefixes",
        "7b0c5639eb2dc379ca80c39074daff99c7695ef03377c1989848cc385d5db798",
    ),
    (
        "big-input",
        "depth",
        "8da821fa8e8e25a2a56b8679598d55fd64c0d7057b8f15ed2057d1673e9fe82a",
    ),
    (
        "big-input",
        "memory",
        "63a97920a035b7b3172ea9b0d0c77ebe4fe68d7e7ea322da2819a1d355e120ab",
    ),
    (
        "float-mixed",
        "warm-cold",
        "ef73369532aa77e2548e80da151a822a889d337a35683ce4d340bdcc265c750b",
    ),
    (
        "float-mixed",
        "events",
        "78ac7e3f65029c32f9ace961d7aa9dfe051c4de3d703e8b1a73508793495c75f",
    ),
    (
        "float-mixed",
        "charge-prefixes",
        "391a07d3bcc6a4ac45443b800aeed4bf82921b76227a6d70681324d3278dc147",
    ),
    (
        "float-mixed",
        "depth",
        "4a6fa741374dfe84eeeff1c3f0c3dba1cf0e1693c3e97e6b93c1ba9df3d482b6",
    ),
    (
        "float-mixed",
        "memory",
        "a4e0d74b6fdff3cde3fd23a3fae2e9f1bea905f6642ee2a5ddd71071df28df0e",
    ),
    (
        "boolean",
        "warm-cold",
        "f11c3b22afaa5539d4a811ea3ad6be6886d20140f5f2f3ec5075b6d992a342c0",
    ),
    (
        "boolean",
        "events",
        "cd62c36e8bf3bdab4d1eae655e561c26e9895d7cf716369343fbe894511f6f41",
    ),
    (
        "boolean",
        "charge-prefixes",
        "d3ba2afa39af6520adf3c62ce68688b67ccb219b0249447910ed168a782800f7",
    ),
    (
        "boolean",
        "depth",
        "2dc2793cb1c290e9b77f36052ad3c7a27701c31f2b7868c6e4f311d24a6b0c61",
    ),
    (
        "boolean",
        "memory",
        "5dc3155ad1c41e631b531821afb37c082bbf9b1b42faf38110752766a53958e8",
    ),
    (
        "text-repeat",
        "warm-cold",
        "6dac8a3688fe583b78e767e0bbf69831d8fdea86888c4f833def84825717dc5b",
    ),
    (
        "text-repeat",
        "events",
        "e494c60bbb71d07643ccfe8321f2a59384d49f33cfebc6645812255873024d53",
    ),
    (
        "text-repeat",
        "charge-prefixes",
        "3d2719972ba1b1d827ddd8459eab7583a111f15d41d5cd16e146d588a52024dd",
    ),
    (
        "text-repeat",
        "depth",
        "55936cd51986298bc758dbc91d009e50b78e050c14b1eabfa336c06fb106c846",
    ),
    (
        "text-repeat",
        "memory",
        "573cc87813e8d0f43ee130ab786bda30b3d8c1e9c96eb6984e7cc09df1272ad5",
    ),
    (
        "list-repeat",
        "warm-cold",
        "128f67e3fd884cfaf5b661d5af9beecd1f599d5823e17c637fad8a4f3ecddbd4",
    ),
    (
        "list-repeat",
        "events",
        "f56b62bd04668a647b183bc83f23e0bc2faf1ab754a72204aa1d7f98ca1d3748",
    ),
    (
        "list-repeat",
        "charge-prefixes",
        "f369f1a22cde195ae61a377df64ee9031a320dd6567dbb3bd9ed803fda5c7209",
    ),
    (
        "list-repeat",
        "depth",
        "a4d2a2e4671b49d64480898b2db1e0314f3549bcf9c1742d63c8959835ed6929",
    ),
    (
        "list-repeat",
        "memory",
        "33194c2dd7419c5fabee9eaf086b26aa3430390aab88eb9d33d8ee3247ecbde6",
    ),
    (
        "tuple-repeat",
        "warm-cold",
        "34690d18f92681f810efd858555d091c767d931beae90d0a9f21264086675130",
    ),
    (
        "tuple-repeat",
        "events",
        "f7dffc3dbcb20b61829bb74661d53ecebbeb9e7249c574cbc4c784f2dac0ffb5",
    ),
    (
        "tuple-repeat",
        "charge-prefixes",
        "a67334fd40f0bb03d9c62b854459b295fdf9e05ab3137f96b8afac77780de2aa",
    ),
    (
        "tuple-repeat",
        "depth",
        "9887b6fc775efd1543b2c08765640b68a7c5224a3cd05f415e352eb3210a1173",
    ),
    (
        "tuple-repeat",
        "memory",
        "db56c7022704fa0d596de854c135bd3514bafd87af717af0814139cd3074142b",
    ),
    (
        "type-error",
        "warm-cold",
        "0ca496b4379776dedfaeae99acb1b81e6db485f4ea497b1b733410af186f1cd6",
    ),
    (
        "type-error",
        "events",
        "64c1df4deb564126e59cd707744de41b039b5a2fe44bf2dd75e041049eeb7d98",
    ),
    (
        "type-error",
        "charge-prefixes",
        "838817cfe5334468d0892d262cb107c26176245d2abe38f9a640d6aa1d165601",
    ),
    (
        "type-error",
        "depth",
        "292c546a063874980df3a815488f3ebe944fea652df5ae72acace9411b690e37",
    ),
    (
        "type-error",
        "memory",
        "6a4fca287bc6af42668f344b4caa6699d88fc5c0c49f35b9cbbba3b04fd7142a",
    ),
    (
        "nested",
        "warm-cold",
        "af43b6292a38def14309140c21dbd8c45d7246f17b3cc2a66d3150853cd56b31",
    ),
    (
        "nested",
        "events",
        "1e9c799b3b7d54505a89167acaa3d561bdba36222beba14225384d9845bb3306",
    ),
    (
        "nested",
        "charge-prefixes",
        "5057ec7d75ff0a4b58a5fcdbdb48dc80830024d41dcd55e480c054c4315b7f3a",
    ),
    (
        "nested",
        "depth",
        "921573358a29ef90c3a08e9b8f5555aedc89ef55617a8af1a5b83aa7911f533e",
    ),
    (
        "nested",
        "memory",
        "69e8d24d605c7fde9ed42b0c216fa63eb4e7bb4977b611978f329af0b700a9ee",
    ),
];
const PARENT_MEMORY: &[(&str, u64)] = &[
    ("records", 4104),
    ("words", 5080),
    ("positive-overflow", 4452),
    ("negative-overflow", 4452),
    ("big-input", 4503),
    ("float-mixed", 6560),
    ("boolean", 5131),
    ("text-repeat", 4728),
    ("list-repeat", 6376),
    ("tuple-repeat", 6376),
    ("type-error", 5155),
    ("nested", 6984),
];

const SOURCES: &[(&str, &str)] = &[
    (
        "records",
        "rows = [{\"active\": i % 2 == 0, \"value\": i} for i in range(4)]\nselected = [r for r in rows if r[\"active\"]]\nmapped = [r[\"value\"] * 2 for r in selected]\nprint(sum(mapped))",
    ),
    (
        "words",
        "def product(a, b):\n    return a * b\nprint(product(7, 6))\nprint(product(-3, 9))\nprint(product(0, -2))",
    ),
    (
        "positive-overflow",
        "def product(a, b):\n    return a * b\nprint(product(9223372036854775807, 2))",
    ),
    (
        "negative-overflow",
        "def product(a, b):\n    return a * b\nprint(product(-9223372036854775808, -1))",
    ),
    (
        "big-input",
        "def product(a, b):\n    return a * b\nprint(product(10000000000000000000000000000000000000000, 2))",
    ),
    (
        "float-mixed",
        "def product(a, b):\n    return a * b\nprint(product(2, 1.5))\nprint(product(1.5, 2))\nprint(product(-0.0, 2))",
    ),
    (
        "boolean",
        "def product(a, b):\n    return a * b\nprint(product(True, 3))",
    ),
    (
        "text-repeat",
        "def product(a, b):\n    return a * b\nprint(product(\"ab\", 3))\nprint(product(2, \"x\"))",
    ),
    (
        "list-repeat",
        "def product(a, b):\n    return a * b\nprint(product([1, 2], 2))",
    ),
    (
        "tuple-repeat",
        "def product(a, b):\n    return a * b\nprint(product((1, 2), 2))",
    ),
    (
        "type-error",
        "def product(a, b):\n    return a * b\nprint(product({}, 2))",
    ),
    (
        "nested",
        "def product(a, b):\n    return a * b\ndef wrap(x):\n    return product(x, 3)\nprint(wrap(7))",
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
fn word_products_preserve_exact_parent_observations() {
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
        if matches!(*id, "words" | "nested") {
            assert!(
                lash_kernel_vm::testing::resident_accesses() > before,
                "the word answer must run: {id}"
            );
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
        // Every candidate uses the exact bounds recorded on the parent.
        let high = PARENT_MEMORY.iter().find(|entry| entry.0 == *id).unwrap().1;
        groups.push((
            "memory",
            std::iter::once(0)
                .chain((high.saturating_sub(512)..=high + 256).step_by(16))
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
                .find(|entry| entry.0 == *id && entry.1 == group)
                .unwrap()
                .2;
            assert_eq!(hex, expected, "{id}; {group}; exact parent8e38dc48da");
        }
    }
}
