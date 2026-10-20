//! Reviewed K-CHG-007 loop bounds for the shipped package audit.
//!
//! Each entry pins the full definition identity, including dependencies and
//! charge formula. Re-review changed definitions; never resolve these by name.
//! Input-sized loops whose extent can reach guest code or awaits need
//! continuation metering; unchanged callback-free inputs can supply bounds.

use std::collections::BTreeMap;

use lash_kernel_doc::FunctionId;

pub(crate) fn reviewed_bounds() -> Result<BTreeMap<FunctionId, String>, String> {
    let mut bounds = BTreeMap::new();
    for &(identity, name, reason) in REVIEWED {
        let function = FunctionId::parse(identity).map_err(|error| {
            format!("invalid reviewed FunctionId for {name}: {identity}: {error}")
        })?;
        if bounds
            .insert(function, format!("{name}: {reason}"))
            .is_some()
        {
            return Err(format!(
                "duplicate reviewed FunctionId for {name}: {identity}"
            ));
        }
    }
    Ok(bounds)
}

// The names are review labels only. The audit matches exclusively by identity
// and refuses unused evidence, so an updated body cannot inherit an old proof.
const REVIEWED: &[(&str, &str, &str)] = &[
    (
        "f27458dc84924d773fc034790a9c45001701c35f9a9424db95cc3682940a34d9",
        "collection.enumerate",
        "Argument-size: the callback-free scan visits each xs member once without mutating xs; writing a fresh output cannot change the input, including on error.",
    ),
    (
        "efb76d65f63fa5b890c80d863df5f033917719d0a28f970f8a08212860534cb5",
        "collection.range",
        "Result-size: validated nonzero integer step appends exactly one element per test; scalar bounds cannot change and validation errors precede either loop.",
    ),
    (
        "766c65050ab4bb7d1ccae0b214ecac5b6fb39743be5128a62e7453522e3212dc",
        "collection.sum",
        "Argument-size: the scan never mutates xs or calls guests; numeric addition may refuse, but visits at most the unchanged input member count.",
    ),
    (
        "e19416b69d7d09b68896f2747decfeb9f3e969314ff5cb7be9fe6952b9a3c8b4",
        "collection.zip",
        "Argument-size: captured n/m cap the increasing index by min(len(xs),len(ys)); neither input is mutated and no guest code runs.",
    ),
    (
        "91f3ef243fd270a0b967358c7da81a033131b183bc3e7d83f36fbbc1847fd1a6",
        "dataframe.data_quanta.check_shape",
        "Argument-size: generated shape loops visit x.plan and each step.exprs without mutation, guest dispatch or awaits; even stale-value refusal is bounded by the unchanged input graph.",
    ),
    (
        "0580749406f9130069d5791a53e845117be519aa2bf7c28e124e7f4eb9d6880c",
        "dataframe.data_quanta.reduce_by_key",
        "Argument-size: generated prologues scan keys and aggregates once, checking each expr without guests or mutation; shape errors shorten these unchanged argument-list scans.",
    ),
    (
        "bb8eafc75a1e1e08d008f27efdd15d86fc06d22526773fd1f87846deeeb9d763",
        "dataframe.data_quanta.sort",
        "Argument-size: the generated prologue scans keys once with callback-free expr checks; input is unchanged and stale-value refusal can only shorten the scan.",
    ),
    (
        "2dd58e4bfe727e14de81d5a6ed60c9fa48a2df2d465caaa82558e7a1240786df",
        "list.insert",
        "Argument-size: j strictly decreases from the captured list length to validated i; the one append only increases final xs size, with no guest callbacks.",
    ),
    (
        "95f02053d8bff2a9e13f28197dc90d4b30d1c64f979a1598425e1bc2824f890d",
        "py.all",
        "Argument-size: py.to_list takes a private entry copy; py.truth cannot call guests or mutate it, and early return/error only shortens the scan.",
    ),
    (
        "f0db12347dd844a6466dfa458a2ab20be9057c00991dec7c189d2b219b79f119",
        "py.any",
        "Argument-size: py.to_list takes a private entry copy; py.truth cannot call guests or mutate it, and early return/error only shortens the scan.",
    ),
    (
        "07f338d1310160ee2c456de9a51ed2675b12b788833ce20c512a80f376c9c097",
        "py.enumerate",
        "Argument-size: py.to_list snapshots values; each copied item is visited once, with no guest callbacks and no mutation of the snapshot.",
    ),
    (
        "2ad9e4c43da5f8cd07cca24f5cc9e78dbe25952b989f7528f5cc4fe436314f27",
        "py.extreme",
        "Argument-size: items is the immutable argument tuple or a private py.to_list copy of its sole iterable; comparisons do not call guests and refusal only shortens the scan.",
    ),
    (
        "591cd7733b4de6a472d9070757c3477461ce2c01170971828c8ebd4eb4b07b6d",
        "py.for_in.open",
        "Fixed-algorithm: the only loop scans five literal kind names, returning before any guest iteration is opened.",
    ),
    (
        "d8ca5ae666e886eff9c80e101533959066f710d67d1db2acefb154d428a158ee",
        "py.format",
        "Fixed-algorithm: its three loops scan literal tables of five integer codes, five fractional codes and four radices; conversion/refusal cannot extend those tables.",
    ),
    (
        "8ca420429e61cdf8489549cc9ea2fb4bc8686f064c3ba2ec8a568b4086119955",
        "py.iadd",
        "Argument-size: py.to_list snapshots the source before appending to the destination, including self-aliasing; no callback can grow the private snapshot.",
    ),
    (
        "8568a89787fc2b2c1b78c4c6edd9eef09122ca4c1b9be2d6320d501dcbecacdb",
        "py.int",
        "Fixed-algorithm: the sole loop checks three literal radix-prefix pairs; conversion and catch paths cannot grow that table.",
    ),
    (
        "867f1cda2de2e2b42f6c261c841bf5a5dec618724120904db0fcee8e4d076fef",
        "py.method.count",
        "Argument-size: the list/tuple scan uses kernel equality, not guest equality, and never mutates self; unsupported kinds and text take other paths.",
    ),
    (
        "02883f7a5bc3bacf9c88f250f066c7f85d1eeee84b9bdaf7497ba05cda574dd9",
        "py.method.difference",
        "Argument-size: the loop visits unchanged self once with primitive membership tests and fresh-output writes; py.to_set and the loop call no guest code.",
    ),
    (
        "d57d42356130a79d0b6a97f0cb5e445313b4c1066a09841ae1d7fb63d271fd99",
        "py.method.extend",
        "Argument-size: py.to_list snapshots the source before appending to the destination, including self-aliasing; no callback can grow the private snapshot.",
    ),
    (
        "054debe5c9fc7cb4486e5ba8c978d5b53aabd943b8091804e9a6b6765b0024e7",
        "py.method.pop",
        "Fixed-algorithm: the sole loop removes and immediately returns its first set member; even an arbitrarily large receiver has at most one visit.",
    ),
    (
        "a87a221806e581f3306c833885d5107af4ff8ff07476809bb19feffeb72f0ff1",
        "py.method.reverse",
        "Argument-size: list.copy(self) is private and position increases to its captured last index; writes preserve receiver length and errors only shorten the walk.",
    ),
    (
        "5886b4051c3c981decd1d3199ec8fb1df8a78f62825422e11d04564dbfe0096e",
        "py.method.split",
        "Argument-size: the whitespace path scans immutable self code points once; word buffers are private and classification has only fixed-table work.",
    ),
    (
        "72f17c2ff3a426d3884926670a62e82c56c3b0bbff9c30bf5039b8b91a8c95f1",
        "py.method.update",
        "Argument-size: py.to_dict/py.to_list copy other before either loop; destination writes cannot grow those private copies, even when self aliases other.",
    ),
    (
        "3536552910167f5f607c7e19dfffb761bdd168d6de0ccd7a0863d62d3a828c97",
        "py.mul",
        "Result-size: the unmetered inner scan appends once per source item without callbacks or source mutation; total visits equal produced elements, and empty repeats use the metered outer test.",
    ),
    (
        "3c7bb2bf926bb115392798ae7d964df1f210b5dcd5fe0ebfa71b26a27781e4a6",
        "py.number_text",
        "Argument-size: trim and code-point conversion scan immutable value once; per-point Unicode digit lookup is a fixed table and no guest code runs.",
    ),
    (
        "a4f9059cf50515d434894bb020d2db76b3f174dd0448a803120ab8aad54f9dab",
        "py.number_underscores",
        "Argument-size: text.split snapshots immutable digits; the loop visits each character once and separator errors only stop it earlier.",
    ),
    (
        "d7c9e36ad51e265806e58d2175e6becb8182e82ad10d0ec1f357d3aaeec075d5",
        "py.range",
        "Fixed-algorithm: the sole local loop checks the three literal scalar bounds; output construction is delegated to collection.range.",
    ),
    (
        "9bf17adf3ba65d63f32c33d6ff9f5c7fa21054d7276388c44471e5cae23b05cb",
        "py.reversed",
        "Argument-size: py.to_list copies values and position decreases from its last index to zero; no guest code or snapshot mutation occurs.",
    ),
    (
        "c73ea9a4e884b851a3edc8ccaa61ef1e5fafefd7fffaf4c52295f78c66cdca56",
        "py.set_op",
        "Argument-size: union/intersection/difference scans left and/or right without mutating either or calling guests; at most len(left)+len(right) visits across the chosen branch.",
    ),
    (
        "1e6057618f081eeeac2ec7ee944d6995781f6f8a4753f40a9601fd83162bf0d0",
        "py.show",
        "Argument-size: args is unchanged while py.str renders each member; its only applied closure is authored by py.repr and cannot call guest methods or mutate args.",
    ),
    (
        "a01b82f38cf5eea4bc0dcbef9a41e89fb8317755ca8dc7dd2701f3ce8f9b2de6",
        "py.slice",
        "Argument-size: the three-bound validation table is fixed; normalized nonzero stride visits at most len(target) elements, then at most that many picked items, without callbacks or input mutation.",
    ),
    (
        "ee8eee06a56bf0ee29a54182b43bc194d8451a5f1dafe9ed1c2064f18dc203e7",
        "py.sub",
        "Argument-size: the sole set-difference scan visits unchanged left once; primitive right membership and fresh-output insertion cannot call guests or grow left.",
    ),
    (
        "5b04378d1b8001145efd58ca3e4c660e7a11f9db28bd32a6e80421f95c67f832",
        "py.sum",
        "Argument-size: py.to_list copies values; py.add performs primitive arithmetic/concatenation without guest dispatch or source mutation, so errors only shorten the scan.",
    ),
    (
        "65af3ea6af15b1bce52820c3712d9201cfe08e38f5b24da2657c7574c124685b",
        "py.text.decimal",
        "Fixed-algorithm: the loop visits only the literal Unicode decimal-zero table, with at most its fixed member count even for invalid code points.",
    ),
    (
        "01878cd4854f27491d88ce96898abd21b4fc2fee18220ff44d09a29f0bf629fe",
        "py.text.printable",
        "Fixed-algorithm: low/high binary-search a literal Unicode interval table, strictly reducing the finite interval each test; no guest code runs.",
    ),
    (
        "af1d40cacc4443a07b1d50b3945c910de40d8fb37a9a80e7ab1542043a044974",
        "py.text.whitespace",
        "Fixed-algorithm: low/high binary-search a literal Unicode interval table, strictly reducing the finite interval each test; no guest code runs.",
    ),
    (
        "a5473618f0e9fd2726bf23653f6a1a1553d9779addd065432d53dd6187668fe2",
        "py.text_repr",
        "Argument-size: the loop visits immutable value code points once; escaping has fixed-width choices and no guest dispatch, including refusal paths.",
    ),
    (
        "f7f4e406e72cecf8ba7a43e8bf6277b05b20995ec82bf6329684e990c2c59160",
        "py.to_dict",
        "Argument-size: py.to_list snapshots value before iteration; each pair conversion is callback-free and invalid pairs stop the scan without growing the snapshot.",
    ),
    (
        "6634ac2c86e34788447b30f9dbd9862f488b08abdbbbc48329c8188bcb53c6ad",
        "py.to_set",
        "Argument-size: py.to_list snapshots value; each copied item is visited once, with writes confined to fresh output and no guest callbacks.",
    ),
    (
        "6456c7b5aad7bd33ca666fa1c08ce6786c389fc009bf40b632be61282792d3d9",
        "py.to_tuple",
        "Argument-size: py.to_list snapshots value; each copied item is visited once, with writes confined to fresh output and no guest callbacks.",
    ),
    (
        "459723b3c0856cab736b1285399ddfd1c5c739dc08c2c1d81891d1ca02287688",
        "py.zip",
        "Argument-size: source conversion is callback-free and inputs are unchanged; outer scans total len(sequences), with at most shortest times len(sequences) nested cell visits, bounded by their deep input size.",
    ),
    (
        "61a28ca87ba1cb77599cc7f745909416fb7859049602e02252bc00be85026a96",
        "ts.arguments",
        "Argument-size: items is unchanged while one property is written per element to a fresh private record; numeric index spelling cannot call guests.",
    ),
    (
        "7ea433237e6399b1f86ac8245d9526c0b161e2a904a511d00c4c47d72215f879",
        "ts.array.concat",
        "Argument-size: sources snapshots this and args; nested length-capped array reads with numeric keys do not call guests, and all appends target a fresh output, leaving inputs unchanged.",
    ),
    (
        "6907d83fd889c30599f170b9eaf30d1b045e4aca1351e82e4c327a3b11a2f557",
        "ts.array.searchable",
        "Argument-size: the index increases through unchanged this; primitive-kind checks neither call guests nor mutate the receiver, and an unsupported element returns early.",
    ),
    (
        "2f445ba92acb8f56c4ac6c7e4054c7ded27c2a07018bf55a9e8287bc7574cd8a",
        "ts.assign",
        "Argument-size: ts.keys makes a private own-key snapshot; Text keys make ts.get callback-free, and destination writes cannot extend the snapshot or delete source fields.",
    ),
    (
        "25bb0b6472f071a3dbc77b15868061217888842c5100b1bce3f95a1bd022d9a7",
        "ts.bitwise32",
        "Fixed-algorithm: both integers are reduced modulo 2^32 before the loop and floor-halved each visit, so at most 32 visits regardless of operand magnitude.",
    ),
    (
        "ac89e50257ae69e06fe10e8faa4c6567d50315aca6fcecee2a967bdc8b4af23b",
        "ts.boundary",
        "Argument-size: list/record/map.copy_deep makes a private graph before walking; inner scans visit its children without guest dispatch, and the outer worklist already declares metered 2.",
    ),
    (
        "9a7771698d7b651fe6394b11fdfaecb6564a69f1ed6f71d39149847a80fefdec",
        "ts.delete",
        "Fixed-algorithm: the only loop scans the two literal function-property names after key coercion; guest code cannot extend that table.",
    ),
    (
        "690182cf08e5924b669d1ea7c32d0fc1f17dc7fb8384ead9df28de52e0dfe9f1",
        "ts.exotic.iterate",
        "Argument-size: map/set scans only decode identity keys and build private output; neither guest dispatch nor source mutation occurs, so the unchanged input entry count bounds visits.",
    ),
    (
        "280d01caa6371cecaac5f9c70d17f653af5bf162012429fc657611440e942a9e",
        "ts.exotic.set",
        "Fixed-algorithm: the loop scans keys of the private url.whatwg.update result state, whose URL component field set is fixed; no guest code runs inside it.",
    ),
    (
        "66243008bcfdd0fd72283a81388aea4975e4ffd5134940b8d7f5372b58af3ae2",
        "ts.keys",
        "Argument-size: no guest code runs; record/error key snapshots and unchanged text/list lengths bound scans, with at most n squared moves for insertion-sorted numeric keys.",
    ),
    (
        "14f366a172867cb501b6b447a07a7b074e1a0251be386ad664c5f9b23c6a3e69",
        "ts.map.entries",
        "Argument-size: receiver validation precedes a callback-free scan of unchanged this; identity-key decoding and fresh-output writes cannot extend the map.",
    ),
    (
        "d43f9da161b5226653d4634ebfe2fb4a90d6d0a803266a51ca2572556485ed06",
        "ts.map.keys",
        "Argument-size: receiver validation precedes a callback-free scan of unchanged this; identity-key decoding and fresh-output writes cannot extend the map.",
    ),
    (
        "c0a35be9cf735da94e10e2fdb28d15ea459553c87cca8396f1e052e8c95b84ad",
        "ts.map.values",
        "Argument-size: receiver validation precedes a callback-free scan of unchanged this; identity-key decoding and fresh-output writes cannot extend the map.",
    ),
    (
        "45b78310c7c14a366905519c1cb0dad6f323651c378aad2b003957b0062438b7",
        "ts.math.clz32",
        "Fixed-algorithm: ToInt32 is reduced modulo 2^32, then floor-divided by two, giving at most 32 visits independent of guest coercion output.",
    ),
    (
        "6e71dd7bbe4bc1253824508495455a8d96782a81458ab349021c8f13954c7781",
        "ts.math.fround",
        "Fixed-algorithm: after finite/nonzero guards, doubling binade from 2^-126 to a finite binary64 magnitude needs at most 1150 visits; no guest code runs inside.",
    ),
    (
        "485ed82cdabbacc595702b5b52055b798f96d6c8dcaba52e723c4da99f00f2a4",
        "ts.number.parse_decimal",
        "Argument-size: all three scans increase one index through immutable source text; malformed digits/exponents stop or rewind once without repeating any scan.",
    ),
    (
        "3849314fcefb23b8a9c499f62aabfd6e57105165d50eaa7a728b7704fab849cd",
        "ts.number.parse_int",
        "Argument-size: each visit advances through immutable source by one character; invalid digits stop the scan and accumulated integer growth is capped at 1e308.",
    ),
    (
        "837280875267b97dfe4d629a8cd1d67d60ccfc0d543f2a6514987ba17856477b",
        "ts.number.parts",
        "Argument-size: the only loop removes trailing decimal zeros from an immutable Int spelling, whose digit count is bounded linearly by the integer word-size; floats take the loop-free branch.",
    ),
    (
        "8d4487ce7f204174eed506cb0db46f5578e0b36ebdace6e967d11506eaf101b2",
        "ts.number.toString",
        "Fixed-algorithm: after radix and finite guards, the only loop increments count and explicitly breaks at 1100, regardless of coerced radix or fraction.",
    ),
    (
        "e044e97f4ad246e2d4c2f31c062987f3ced1024e2c9d7bbb6455d9425d5471bb",
        "ts.object.assign",
        "Argument-size: ts.rest snapshots sources and own_keys snapshots each key list; ts.get with Text keys does not call guests, and writes cannot extend either snapshot.",
    ),
    (
        "82e1b5aad1cb7b1a82e2b26d9c8baed89b89f6c0df1b99af9da9bb0ef0b1e102",
        "ts.object.entries",
        "Argument-size: own_keys snapshots target keys before iteration; ts.get with Text keys does not call guests, so each key is visited once even on refusal.",
    ),
    (
        "95d311b780b1952aaabd4a40be32b93863de320cc74af1e9e1c1d6fbf9c7939b",
        "ts.object.isPrototypeOf",
        "Fixed-algorithm: the only loop checks ten literal receiver tags; all guest coercions precede the table scan and cannot extend it.",
    ),
    (
        "0336ba6e1ca8fabfa71c0123e6fdea457d8445806abbf196e572181ac7e7c3b2",
        "ts.object.own_keys",
        "Argument-size: ts.keys returns a private key list; insertion sort strictly decreases position and needs at most n squared moves, followed by one private remainder scan.",
    ),
    (
        "d8224d9fae31686b38b0d56b6b332a9e94c40421f48457638709c7f291d07f20",
        "ts.object.values",
        "Argument-size: own_keys snapshots target keys before iteration; ts.get with Text keys does not call guests, so each key is visited once even on refusal.",
    ),
    (
        "53db560d550e2fe9ad845b269a3f7f21d8ea190c3c4f61607cb62458a6807584",
        "ts.object_rest",
        "Argument-size: taken and ts.keys(source) are scanned once each with Text keys; neither membership tests nor data-only ts.get calls guests or mutates either input.",
    ),
    (
        "0ac2376081f53b9524b0d3df74d59d37b6a3f6946504bd421a26f5c308733264",
        "ts.promise.decided",
        "Argument-size: states is scanned once without guest dispatch, mutation or awaits; subsequent mode selection is loop-free.",
    ),
    (
        "ec4fee5a46bb2d63665dd02de716b63bf630c4877fc96abc3b23525960656a0f",
        "ts.regexp.escaped_source",
        "Argument-size: pattern is immutable Text and position advances by a complete scalar (one or two UTF-16 units), so even refusal cannot exceed its input length.",
    ),
    (
        "8ae5662f9021ef7af090ef451785554985079c643964d1f88b41afe268f14bf1",
        "ts.regexp.match_record",
        "Argument-size: match.groups is scanned once and record.keys(named) is a private snapshot; replacing null slots preserves sizes, with no guest code or awaits.",
    ),
    (
        "d1ebfcf7c45fa6a5fa8e08cb3975359d66fcddca1d531ad86035fd7a088e913f",
        "ts.rest",
        "Argument-size: every successful index read advances position through unchanged items; an invalid start refuses before further visits, and hole normalization cannot call guests.",
    ),
    (
        "1401cf60999eda184c4860c001e8f2b5f6ea750922e68a58a9f501992dda4f4e",
        "ts.set.entries",
        "Argument-size: receiver validation precedes one scan of unchanged this; identity decoding and private-output writes cannot call guests or mutate the set.",
    ),
    (
        "8cfd70fd26c3bd9c536482bbb2f25b0ad7658848b6cf6bec2cdf278d0f3b8053",
        "ts.set.values",
        "Argument-size: receiver validation precedes one scan of unchanged this; identity decoding and private-output writes cannot call guests or mutate the set.",
    ),
    (
        "640c6cadf06f30f4542de9573922f28cddd121355dca7b3673f76003acf0799b",
        "ts.spread",
        "Argument-size: both loops only read unchanged parts and append to fresh flat; total element visits are bounded by the sum of input list lengths, even with aliases.",
    ),
    (
        "6edd667ddf772570f6c0da88bc24edf40a3bea658de30d4a87a4711cdc2a992c",
        "ts.string.substitute",
        "Argument-size: replacement is immutable Text; index strictly advances past each dollar token and never exceeds its UTF-16 length, including slice refusal paths.",
    ),
    (
        "35e088c67549ba9d69a338952219a6bbd4abd0d0c8a69a99583b1d10e99912e3",
        "ts.uri.decode",
        "Argument-size: text.split snapshots immutable input at percent separators; each chunk is visited once and malformed hex/UTF-8 only terminates the walk early.",
    ),
    (
        "e619c844468f0cfb53f3a47a20fb7c2ee20a9503e20b92459ad52ac46ab97eb3",
        "ts.uri.encode",
        "Argument-size: immutable input is consumed by scalar with increasing UTF-16 index; each scalar creates a private list of at most four UTF-8 octets.",
    ),
    (
        "efe133dd07636de8ea7e6eabc3d25937edab65f95a30e9b3ad4e0bfc2cb1bbcf",
        "ts.url_search_params.entries",
        "Argument-size: this.pairs is scanned once without callbacks or mutation; fresh-output writes preserve the unchanged deep receiver bound, and invalid pair access only shortens the scan.",
    ),
    (
        "05931e9de8dc6fe925190791e6e238a61c00adaf9d8fce1163024d45a6ce667c",
        "ts.url_search_params.keys",
        "Argument-size: this.pairs is scanned once without callbacks or mutation; fresh-output writes preserve the unchanged deep receiver bound, and invalid pair access only shortens the scan.",
    ),
    (
        "3c8ed65e6fe4c45d4ee0da497c064fb06a5ae951207050d97d28f8e5d291969e",
        "ts.url_search_params.refresh",
        "Fixed-algorithm: the loop scans a private url.whatwg.update result state with a fixed URL component field set; no guest code runs inside it.",
    ),
    (
        "007c665ea084f58500599937879f0eaee4a51853b2e4ee0978f6c82203b4d28d",
        "ts.url_search_params.sort",
        "Argument-size: insertion sort increases i and strictly decreases j over unchanged-length this.pairs, giving at most n squared visits; text comparisons and refresh call no guests.",
    ),
    (
        "a80b23218790356aa8721585cd63ce8299b1f32aeccfb67d32b582728b609d02",
        "ts.url_search_params.values",
        "Argument-size: this.pairs is scanned once without callbacks or mutation; fresh-output writes preserve the unchanged deep receiver bound, and invalid pair access only shortens the scan.",
    ),
];
