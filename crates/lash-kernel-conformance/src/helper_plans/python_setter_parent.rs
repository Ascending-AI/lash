//! K-LIB-006 / K-CHG-007: parent observations for resident map writes.
use super::*;
use lash_kernel_vm::KernelMachine;
use serde::Serialize;

// Exact parent 12f0d3e1211f6a3c5229add517b2a23c7ee50dac, recorded before this
// cut, for the groups that run warm only. A cold run is held to its warm
// partner instead.
const PARENT: &[(&str, &str, &str)] = &[
    (
        "overwrites",
        "charge-prefixes",
        "8fc724260642bf89e70b8c376c7fd6111a4fdff811af343b15af3725675baf25",
    ),
    (
        "overwrites",
        "depth",
        "835486f7a364d914fc1dcda2861d55d27f91a533671e59402ea59cbe37752c07",
    ),
    (
        "overwrites",
        "memory",
        "990d5df35cd266d770cdbc007a3b0559de1737b5abdda9720957ad49f071e990",
    ),
    (
        "new-keys",
        "charge-prefixes",
        "e8e6ec9eee7dfaa6df893f2342ac69fd264f7434e9b10ef1619d736404010c29",
    ),
    (
        "new-keys",
        "depth",
        "9c59b5165851a1d16bc51ee4a7aef6455b9d557280b918a06517110e6cd4d35e",
    ),
    (
        "new-keys",
        "memory",
        "2b1f618b2cae08dd113d606db6b38a3bfc49570f4ec2798b3713aa22a88a1a18",
    ),
    (
        "aliases",
        "charge-prefixes",
        "1a990195b7f3907a577263456fcfb0083388c6203d2bf264e17c8deef2dc0c78",
    ),
    (
        "aliases",
        "depth",
        "f107c6aac49a9598f54c8db56fdc5acdaba8dad51abdf09b097fed33bd44db89",
    ),
    (
        "aliases",
        "memory",
        "3e7dc718a1cfa2697f3cf5aecf9240e5665ef2eb5544c3ff32c0760ec005017b",
    ),
    (
        "self-value",
        "charge-prefixes",
        "c226c88a5075ac42037f0028fd5dae9e7b680afff59b343cff5296c36f356afa",
    ),
    (
        "self-value",
        "depth",
        "690db69be8aade172b8691c1b08ff362aab79ad38dfa945a0a302cf795eed8cc",
    ),
    (
        "self-value",
        "memory",
        "dd65095f0cef7a5facffef5b1ec4de8c1c3eab9ed4446612690e56684e07a794",
    ),
    (
        "nontext-keys",
        "charge-prefixes",
        "eb9c2c0e23ea120971110afadbc0a3543127304d31305d33099e4468e13db54e",
    ),
    (
        "nontext-keys",
        "depth",
        "f90d51660aa1833291f93fb782bdbc81c7bd010d8548312574892f740880260e",
    ),
    (
        "nontext-keys",
        "memory",
        "e37cd671ab6f85dd9a40cee6257add893b7246372d114bdd798e00bc54e5b1ec",
    ),
    (
        "long-key",
        "charge-prefixes",
        "43f8e382792b56622de8d7be4f94ffc808245c9ad703918ff4a75349e7ed28af",
    ),
    (
        "long-key",
        "depth",
        "defa849ee249e4716931e642affdd35f2cea7fde0a3fd3a8501a57845cbce8f3",
    ),
    (
        "long-key",
        "memory",
        "a365f52aa292acbca2d914c27f7a8fd335d11f372a36c16cdad74869aa430784",
    ),
    (
        "list",
        "charge-prefixes",
        "802ed395179ab08c9d18f1df788205d9b1d885f30f3c883179d6443d334e8fb1",
    ),
    (
        "list",
        "depth",
        "f4a6479beac67f7688a984781dc8eb79f29b4aa9b3557d04fd75738a2f0508f5",
    ),
    (
        "list",
        "memory",
        "eb10ad473c230c5e987838eab4f50d22f96bef61e3581569a9095c91cea6b7a7",
    ),
    (
        "bad-list-index",
        "charge-prefixes",
        "2bfbdbd0fd966d1ad0c66d5150433e31a08f749b05ad2caf9f3f28a7e9f63e9e",
    ),
    (
        "bad-list-index",
        "depth",
        "069c3fac87c137ee108d58f266d377217744de0125b4851a60e2c4d6ce0b2695",
    ),
    (
        "bad-list-index",
        "memory",
        "3de6735edcc08887d6c4e58f80fdafd6ade3f9124b8bfe452ec70184c7d849b2",
    ),
    (
        "immutable",
        "charge-prefixes",
        "51e870e805bf08d8122b860c87728aeeba607448762c2b97b75fd89045b0b202",
    ),
    (
        "immutable",
        "depth",
        "a19ea28cfdec963d636d19492cc88d68ab8a5d020cc97d4290561c5b2ba93b3d",
    ),
    (
        "immutable",
        "memory",
        "dcc2847d715baf3ea6795a6ea4ac69d076b3c3a9f299e6ba25e776520020095d",
    ),
    (
        "nonhashable-key",
        "charge-prefixes",
        "1a09be60d99da3b91f7ca0fc5129174f7415396c94bc68e69f994671c98a93f9",
    ),
    (
        "nonhashable-key",
        "depth",
        "411f6773a5402bcca1aaaa3d36cc0dd55ee545691f5d4ce981b7f5880bd3ea38",
    ),
    (
        "nonhashable-key",
        "memory",
        "14c5e9543378c98a8df1c234b143cc14226015dda57d9dda5ef90a803fb9a6d8",
    ),
    (
        "cold-value",
        "charge-prefixes",
        "ca26767dbcb85051181597f8ea4a1620d7a6107fa012aa3c16b09a47019ec342",
    ),
    (
        "cold-value",
        "depth",
        "412eeef68027c7a9628fcdc4ab7e1598b264fcf97b5254a14d9db74559aea109",
    ),
    (
        "cold-value",
        "memory",
        "5755a48554d922b42f5eebd7cbd99a46353d25246dada1cd0ca6041324dbea1f",
    ),
    (
        "key-effects",
        "charge-prefixes",
        "3f3b8dc1857267882f325381603c074d36abc40a9a769a8a33afbb2dc4fd6529",
    ),
    (
        "key-effects",
        "depth",
        "fca6eab0f2c1b5f2061d3d53de51ef59687e29581685319deb96251eca0aa441",
    ),
    (
        "key-effects",
        "memory",
        "fe1a19d7cd292ac70ae6de2831137b98d3ac50df227f2778e31bc4da50d83974",
    ),
    (
        "nested",
        "charge-prefixes",
        "4ccff99cb762387273d7f2ef3c24ce0e56468d6dc19aaa1feff94aba80088aee",
    ),
    (
        "nested",
        "depth",
        "2cce9d86fd33571b66125ce809a905523d5916ecf00a3c21a680eeefce419f24",
    ),
    (
        "nested",
        "memory",
        "6fce8a953113f302d27932f45e8484aeb7935a0a1608c3c7374b0bd54eea9966",
    ),
    (
        "json-loop",
        "charge-prefixes",
        "f246f5d82477024308f9f97f272492943354a384cd1fd7ea4e604c5a11c5f8b5",
    ),
    (
        "json-loop",
        "depth",
        "742c6a8584def6362d121e7e5f96523fcaf8717176e76bb0e62f1967f61006d4",
    ),
    (
        "json-loop",
        "memory",
        "d5dddb9afaf233e1ed5e0f55579696d4ebac4b97a9c44be34c5a8df5fb54d05f",
    ),
    (
        "value-kinds",
        "charge-prefixes",
        "5b9cf35f5dcbf34672964942a89ba9c81d9acf037ce8081f10d2664517e612c9",
    ),
    (
        "value-kinds",
        "depth",
        "f43c44a98fbda6f8008e1dabc4c58b6e1bff9f3c35df2726ecd1cea7c93a4e78",
    ),
    (
        "value-kinds",
        "memory",
        "a72dd728c73a5bd894119fecdd60ebf91db554c7c3436467549571c49971920c",
    ),
    (
        "tuple-key-cold",
        "charge-prefixes",
        "ef554fd4049352ad47e9994b3ef3935618e845a4aa6262119336c436d4c1ff9b",
    ),
    (
        "tuple-key-cold",
        "depth",
        "e640ef79e23980fa950d6c0f8db2f4a185012ac384c2c7fb1a9f5e8c1cc84fe9",
    ),
    (
        "tuple-key-cold",
        "memory",
        "2d4597cfd72c2590b329c3fb9ae5e5f7619b3498724e81ee3041896d127721d6",
    ),
];
const PARENT_MEMORY: &[(&str, u64)] = &[
    ("overwrites", 5782),
    ("new-keys", 6758),
    ("aliases", 4376),
    ("self-value", 2448),
    ("nontext-keys", 4630),
    ("long-key", 3342),
    ("list", 4344),
    ("bad-list-index", 1474),
    ("immutable", 1343),
    ("nonhashable-key", 799),
    ("cold-value", 4224),
    ("key-effects", 5190),
    ("nested", 9966),
    ("json-loop", 5574),
    ("value-kinds", 5040),
    ("tuple-key-cold", 5160),
];
const SOURCES: &[(&str, &str)] = &[
    (
        "overwrites",
        "r = {\"value\": 0}\nfor i in range(8):\n    r[\"value\"] = i\nprint(r)",
    ),
    (
        "new-keys",
        "r = {}\nfor i in range(20):\n    r[\"key\" + str(i)] = i\nprint(r)",
    ),
    (
        "aliases",
        "a = {\"x\": 1}\nb = a\na[\"x\"] = [2]\nb[\"x\"].append(3)\nprint(a)",
    ),
    (
        "self-value",
        "r = {}\nr[\"self\"] = r\nprint(r is r[\"self\"])",
    ),
    (
        "nontext-keys",
        "r = {}\nr[1] = \"a\"\nr[1.0] = \"b\"\nr[True] = \"c\"\nr[(1, \"x\")] = [0]\nprint(r)",
    ),
    (
        "long-key",
        "key = \"long-key-that-will-not-fit-inline\"\nr = {key: 1}\nr[key] = 2\nprint(r)",
    ),
    ("list", "r = [1, 2]\nr[-1] = 3\nr[0] = [4]\nprint(r)"),
    ("bad-list-index", "r = [1]\nr[9] = 2"),
    ("immutable", "r = (1, 2)\nr[0] = 3"),
    ("nonhashable-key", "r = {}\nr[[]] = 1"),
    (
        "cold-value",
        "payload = [i for i in range(64)]\nr = {\"x\": 0}\nr[\"x\"] = payload\nprint(len(r[\"x\"]))",
    ),
    (
        "key-effects",
        "def key():\n    print(\"key\")\n    return \"value\"\nr = {\"value\": 0}\nr[key()] = 7\nprint(r)",
    ),
    (
        "nested",
        "def put(r, k, v):\n    r[k] = v\nr = {\"value\": 0}\nfor i in range(4):\n    put(r, \"value\", i)\nprint(r)",
    ),
    (
        "json-loop",
        "rows = [{\"value\": i} for i in range(4)]\nfor r in rows:\n    r[\"value\"] += 1\nprint(rows)",
    ),
    (
        "value-kinds",
        "r = {\"x\": 1}\nfor value in [None, False, 2.5, \"text\", (1,), {\"a\": 2}, [3]]:\n    r[\"x\"] = value\nprint(r)",
    ),
    (
        "tuple-key-cold",
        "key = (\"a\", (\"b\", 1))\nr = {key: 0}\nr[key] = 1\nprint(r)",
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
    /// The end with the session carry's page reference dropped: a reopened
    /// run seals its carry into a different page.
    #[serde(skip)]
    answer: Option<lash_kernel_vm::End>,
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
                answer: None,
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
            let mut answer = end.clone();
            if let lash_kernel_vm::End::Finished(finished) = &mut answer {
                finished.carry = None;
            }
            return Observed {
                end: format!("{end:?}"),
                prints: world.prints,
                stops,
                states,
                events,
                memory: machine.meters().memory,
                answer: Some(answer),
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
fn map_writes_preserve_exact_parent_observations() {
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
        if *id == "nested" {
            assert!(
                lash_kernel_vm::testing::resident_accesses() > before,
                "the map-write answer must run: {id}"
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
            if matches!(group, "warm-cold" | "events") {
                // Each warm run is followed by its cold partner. A reopen may
                // hold less resident memory, and only the cold run records
                // states; the answer, prints, stops and charges must agree.
                for pair in observations.chunks(2) {
                    let [warm, cold] = pair else { unreachable!() };
                    assert_eq!(warm.answer, cold.answer, "{id}; {group}; end");
                    assert_eq!(warm.prints, cold.prints, "{id}; {group}; prints");
                    assert_eq!(warm.stops, cold.stops, "{id}; {group}; stops");
                    assert_eq!(warm.events, cold.events, "{id}; {group}; events");
                }
                continue;
            }
            let bytes = serde_json::to_vec(&observations).unwrap();
            let digest = lash_kernel_doc::heap_values::key_digest(&bytes);
            let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
            let expected = PARENT
                .iter()
                .find(|entry| entry.0 == *id && entry.1 == group)
                .unwrap()
                .2;
            assert_eq!(hex, expected, "{id}; {group}; exact parent12f0d3e121");
        }
    }
}
