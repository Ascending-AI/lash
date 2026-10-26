//! Reviewed K-CHG-007 loop bounds for the shipped package audit.
//!
//! Each entry pins the full definition identity, including dependencies and
//! charge formula. Re-review changed definitions; never resolve these by name.
//! Input-sized loops whose extent can reach guest code or awaits need
//! continuation metering; unchanged callback-free inputs can supply bounds.

use std::collections::BTreeMap;

use lash_kernel_value::names::FunctionId;

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
        "ce24286d99e4ce06d866a714c337c47f4e700cbd0c2426fc7d3c03badd8cc5fc",
        "py.bytes_len",
        "Argument-size: Bytes is immutable and length increases by one after each successful bytes.at; the input length bounds the successful reads and one index error terminates the scan, without guest calls or mutation.",
    ),
    (
        "1a00364faa5facb72365cf82d88e00af4173bf83525adc40ab36dff0e645994e",
        "py.method.to_bytes",
        "Result/argument-size: length is a validated nonnegative immutable integer; each increasing index appends one private octet. Success produces length bytes; overflow can occur only when length is bounded by self's word extent. No guest call can grow the loop, and the quadratic formula covers base-256 division work.",
    ),
    (
        "cbaef2eef516cb2e918b83d4fc720dd9911464c98f7cf8671681780b66efac3a",
        "py.int.from_octets",
        "Argument-size: the loop visits each already materialized octet once, with primitive integer work and no callback or mutation of octets. The quadratic deep(octets) formula also bounds leading zeroes and sign extension; the iterable-producing wrapper is statement-metered.",
    ),
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
        "54819d41cf8f046723240c72206542ff430d4b330a877186f9394e685f20e76c",
        "dataframe.data_quanta.check_shape",
        "Argument-size: generated shape loops visit x.plan and each step.exprs without mutation, guest dispatch or awaits; even stale-value refusal is bounded by the unchanged input graph.",
    ),
    (
        "9b76034b719078cdb5f33ec6780c7e64e1e26735005bac01c62d374b82097908",
        "dataframe.data_quanta.reduce_by_key",
        "Argument-size: generated prologues scan keys and aggregates once, checking each expr without guests or mutation; shape errors shorten these unchanged argument-list scans.",
    ),
    (
        "4f720a4fba4cdd6de0c3ffc2da18b9114b176d82b425220b13bd93064b6fb31b",
        "dataframe.data_quanta.sort",
        "Argument-size: the generated prologue scans keys once with callback-free expr checks; input is unchanged and stale-value refusal can only shorten the scan.",
    ),
    (
        "2dd58e4bfe727e14de81d5a6ed60c9fa48a2df2d465caaa82558e7a1240786df",
        "list.insert",
        "Argument-size: j strictly decreases from the captured list length to validated i; the one append only increases final xs size, with no guest callbacks.",
    ),
    (
        "c65d63e53210fbadc93a19c5ef6e024d95fff2d1ea620c41efb80850b6775968",
        "py.all",
        "Argument-size: py.to_list takes a private entry copy; py.truth cannot call guests or mutate it, and early return/error only shortens the scan.",
    ),
    (
        "9d6530ef69ed52c98f07cdd764723ccf2ff45a35e4d17632981ca5a225b97dbd",
        "py.any",
        "Argument-size: py.to_list takes a private entry copy; py.truth cannot call guests or mutate it, and early return/error only shortens the scan.",
    ),
    (
        "805b739bc71bfe099060ca64eebae3a3d69b1e57e95f52f4bcbdfbd75a0c46d1",
        "py.enumerate",
        "Argument-size: py.to_list snapshots values; each copied item is visited once, with no guest callbacks and no mutation of the snapshot.",
    ),
    (
        "0bf0936c942d94c89126164b07c7b6e17942672c2af1c58ed5c1301d4cb2354c",
        "py.extreme",
        "Argument-size: items is the immutable argument tuple or a private py.to_list copy of its sole iterable; comparisons do not call guests and refusal only shortens the scan.",
    ),
    (
        "6493a17d666515acdb0be8f73eef0f455873f1508f8bd296c11773152a7b7e3f",
        "py.for_in.open",
        "Fixed-algorithm: the only loop scans five literal kind names, returning before any guest iteration is opened.",
    ),
    (
        "aa44a59f486fecd5d0b2e9363e956115328f89f62eb1a083f635660eba1ea0c3",
        "py.format",
        "Fixed-algorithm: its three loops scan literal tables of five integer codes, five fractional codes and four radices; conversion/refusal cannot extend those tables.",
    ),
    (
        "db032522a59b5ed49f7f09ed109e2de89b7f2899360c89623aa5e9334f178976",
        "py.iadd",
        "Argument-size: py.to_list snapshots the source before appending to the destination, including self-aliasing; no callback can grow the private snapshot.",
    ),
    (
        "03b16fdb3ee8ec4219985ca7717dc1db0d9317c8bd5fcbcfd52e485abba86c76",
        "py.int",
        "Fixed-algorithm: the sole loop checks three literal radix-prefix pairs; conversion and catch paths cannot grow that table.",
    ),
    (
        "9cd5fb7842680a6ca561241de5f0fbce131284e88317ce25a737f241e2c64d2e",
        "py.method.count",
        "Argument-size: the list/tuple scan uses kernel equality, not guest equality, and never mutates self; unsupported kinds and text take other paths.",
    ),
    (
        "8e6f6753ddcbc95f3397fcd1bf62254c8127b498fd663792d41fcc2ce948de1d",
        "py.method.difference",
        "Argument-size: the loop visits unchanged self once with primitive membership tests and fresh-output writes; py.to_set and the loop call no guest code.",
    ),
    (
        "40a305f052b320d86261e837ae29ea8d85adfa78f00a0f84a71832f02a71ad2a",
        "py.method.extend",
        "Argument-size: py.to_list snapshots the source before appending to the destination, including self-aliasing; no callback can grow the private snapshot.",
    ),
    (
        "bad5b3269404dbe872b0aa9d189ab86152859a5e49b0521212e59924c0d2d2cb",
        "py.method.pop",
        "Fixed-algorithm: the sole loop removes and immediately returns its first set member; even an arbitrarily large receiver has at most one visit.",
    ),
    (
        "829fb7d0153f1ddb65b1d53d0abdf19f80bbbfc4f966b4facfb655c5a0b3b94b",
        "py.method.reverse",
        "Argument-size: list.copy(self) is private and position increases to its captured last index; writes preserve receiver length and errors only shorten the walk.",
    ),
    (
        "137812e25d81cafb3c8f88f9f37468eeae3092cc77c27c7371f10dc93e906fec",
        "py.method.split",
        "Argument-size: the whitespace path scans immutable self code points once; word buffers are private and classification has only fixed-table work.",
    ),
    (
        "fc924cfb3b19a5365a79127a88b2447b0d1869b5802e3c9d6b7caec16a6d6806",
        "py.method.update",
        "Argument-size: py.to_dict/py.to_list copy other before either loop; destination writes cannot grow those private copies, even when self aliases other.",
    ),
    (
        "aa398c964e346beb3c96dbb956b9fcefdc73c8d754314920c8fa3b9032deb274",
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
        "ecf535ba709b4a89ee89c82e5d17acf807164fcc74d285b9fb677f465b4dfb57",
        "py.range",
        "Fixed-algorithm: the sole local loop checks the three literal scalar bounds; output construction is delegated to collection.range.",
    ),
    (
        "338486a3135806676d02c4eac0b700e2831df77374cbb9b692fb035913da70e1",
        "py.reversed",
        "Argument-size: py.to_list copies values and position decreases from its last index to zero; no guest code or snapshot mutation occurs.",
    ),
    (
        "91fbb55e9e1b5c8953b4e978d0cc367d67d82cd1548d1709065fd1403931c8a7",
        "py.set_op",
        "Argument-size: union/intersection/difference scans left and/or right without mutating either or calling guests; at most len(left)+len(right) visits across the chosen branch.",
    ),
    (
        "60ed01b1d416227763a9be4beb0442f89dc9738062ce79e3c00109f6c9540957",
        "py.show",
        "Argument-size: args is unchanged while py.str renders each member; its only applied closure is authored by py.repr and cannot call guest methods or mutate args.",
    ),
    (
        "6cc5ba9a39514b6f477f6e4c344f5f0558f1a3ea8974620356609cd02e8f8415",
        "py.slice",
        "Argument-size: the three-bound validation table is fixed; normalized nonzero stride visits at most len(target) elements, then at most that many picked items, without callbacks or input mutation.",
    ),
    (
        "cb9039f5185ac44a3af17e9772ad4e1f2852bbbc0c426870476ebb9a747804a1",
        "py.sub",
        "Argument-size: the sole set-difference scan visits unchanged left once; primitive right membership and fresh-output insertion cannot call guests or grow left.",
    ),
    (
        "9f2f8baabdeef6c409f8f66104ee859ddaa709ad3625e30e3de81567cefc4ee8",
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
        "9d550667761756d19d2b573b654a82d7b1d0f21c073a3155816405cf833f01fa",
        "py.to_dict",
        "Argument-size: py.to_list snapshots value before iteration; each pair conversion is callback-free and invalid pairs stop the scan without growing the snapshot.",
    ),
    (
        "aef478168373bf0394ed727cf32d884a9bdb795a03c6df05f5d6fd6d41772c9f",
        "py.to_set",
        "Argument-size: py.to_list snapshots value; each copied item is visited once, with writes confined to fresh output and no guest callbacks.",
    ),
    (
        "b7daf6c19428dad108d595b86f02956c0bf6d921ddea58872c3c71be2f6d3a10",
        "py.to_tuple",
        "Argument-size: py.to_list snapshots value; each copied item is visited once, with writes confined to fresh output and no guest callbacks.",
    ),
    (
        "1f4a5edb09e03af8353487dd373def11e66ba0d3d24bec33cdb17bff7b4f7fe4",
        "py.zip",
        "Argument-size: source conversion is callback-free and inputs are unchanged; outer scans total len(sequences), with at most shortest times len(sequences) nested cell visits, bounded by their deep input size.",
    ),
    (
        "380779531cfadb38c8cac230416e82e975dbfe9d08703cc1de13af98c73bb2a0",
        "ts.arguments",
        "Argument-size: items is unchanged while one property is written per element to a fresh private record; numeric index spelling cannot call guests.",
    ),
    (
        "bf75b558a144f9fc1461b47fe3639b201fb2163e06a5906aca10f8d9f2051d1f",
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
        "d2160019a5f38f82f2e5a0cd91aa55725258ce3cdf67462bbb61baf8e7397c57",
        "ts.math.clz32",
        "Fixed-algorithm: ToInt32 is reduced modulo 2^32, then floor-divided by two, giving at most 32 visits independent of guest coercion output.",
    ),
    (
        "a1f407bcfbb5aaa2ddfced7addd80c14599d0b8a198e34a215e1c14a5cbc9c03",
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
        "909cd0df1a0c9024f6a8fa1bcdcbaebbd5750b1561528ba7cd7563555f27d1e2",
        "ts.number.ratio",
        "Fixed-algorithm: the entry guard raises RangeError for NaN and infinities; a finite binary64 magnitude becomes an integer after at most 1074 doublings, within the constant 2200 formula, and no guest code runs.",
    ),
    (
        "d3a217cc5b6c591606081ab4b159aedb7595c3922041a3dd5b8bb631a740d21a",
        "ts.number.toString",
        "Fixed-algorithm: after radix and finite guards, the only loop increments count and explicitly breaks at 1100, regardless of coerced radix or fraction.",
    ),
    (
        "bb887c7061ac4287f5f603b21938f6deaa2ce5632952d135293b8543dab80a7c",
        "ts.object.assign",
        "Argument-size: ts.rest snapshots sources and own_keys snapshots each key list; ts.get with Text keys does not call guests, and writes cannot extend either snapshot.",
    ),
    (
        "7a64cd58a5a1e90c950fd83b91be339ea876f95f39b2e9427c9738488574ca04",
        "ts.object.entries",
        "Argument-size: own_keys snapshots target keys before iteration; ts.get with Text keys does not call guests, so each key is visited once even on refusal.",
    ),
    (
        "8be1a4402692a61703af2d3b8ebb49ee12e491e12f61906924e334b008ce1670",
        "ts.object.isPrototypeOf",
        "Fixed-algorithm: the only loop checks ten literal receiver tags; all guest coercions precede the table scan and cannot extend it.",
    ),
    (
        "0336ba6e1ca8fabfa71c0123e6fdea457d8445806abbf196e572181ac7e7c3b2",
        "ts.object.own_keys",
        "Argument-size: ts.keys returns a private key list; insertion sort strictly decreases position and needs at most n squared moves, followed by one private remainder scan.",
    ),
    (
        "a643e25936ad2defc4657edc0a026f28022c635929d6d1700db670dd7f390516",
        "ts.object.values",
        "Argument-size: own_keys snapshots target keys before iteration; ts.get with Text keys does not call guests, so each key is visited once even on refusal.",
    ),
    (
        "53db560d550e2fe9ad845b269a3f7f21d8ea190c3c4f61607cb62458a6807584",
        "ts.object_rest",
        "Argument-size: taken and ts.keys(source) are scanned once each with Text keys; neither membership tests nor data-only ts.get calls guests or mutates either input.",
    ),
    (
        "e0b28326c9a016d8b2582249d54b6cdad4191bd6da5bfdc9780c09da0b6024a1",
        "ts.pad",
        "Argument-size: the copy loop visits each items member once without guest code or mutation of items. The append loop's trip count comes from count's value, which magnitude(count) does not measure, so it is metered.",
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
        "e1966f0f529457742fb199236b2bd3f1007e6f665d5613b62484d13222dc7bb5",
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
