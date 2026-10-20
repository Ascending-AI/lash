// Program matrix and core-witness law inventory.

macro_rules! program_shards {
    (@parts $shard:literal, $parts:expr, $part:expr;) => {};
    (@parts $shard:literal, $parts:expr, $part:expr; $name:ident $(, $rest:ident)*) => {
        #[test]
        fn $name() {
            let group = PARTS[..$shard].iter().sum::<usize>() + $part;
            regions_on_and_off_agree(Some(group), None);
        }
        program_shards!(@parts $shard, $parts, $part + 1; $($rest),*);
    };
    (@core $shard:literal, $parts:expr, $part:expr;) => {};
    (@core $shard:literal, $parts:expr, $part:expr; $name:ident $(, $rest:ident)*) => {
        #[test]
        fn $name() {
            regions_on_and_off_agree(None, Some(($shard, Sweep { part: $part, parts: $parts })));
        }
        program_shards!(@core $shard, $parts, $part + 1; $($rest),*);
    };
    (@extra $group:expr;) => {};
    (@extra $group:expr; $name:ident $(, $rest:ident)*) => {
        #[test]
        fn $name() {
            regions_on_and_off_agree(Some($group), None);
        }
        program_shards!(@extra $group + 1; $($rest),*);
    };
    ($($shard:literal => [$($name:ident),+]),+; extra = [$($extra:ident),+] $(,)?) => {
        const PARTS: [usize; SHARDS] = [$([$(stringify!($name)),+].len()),+];
        const GROUPS: usize = 0 $(+ [$(stringify!($name)),+].len())+ + [$(stringify!($extra)),+].len();
        $(program_shards!(@parts $shard, PARTS[$shard], 0; $($name),+);)+
        program_shards!(@extra PARTS.iter().sum::<usize>(); $($extra),+);
        mod core {
            use super::*;
            $(program_shards!(@core $shard, PARTS[$shard], 0; $($name),+);)+
        }
    };
}

// Each original group also projects a separate core law, preserving its
// repeated exit/session witnesses and smith schedules without stacking them
// on a program matrix. Program matrices are dealt by cost across all listed
// program laws, with large matrices split into disjoint sweeps. Membership is
// authored here once; Deal derives its capacity from this same inventory.
program_shards! {
    0 => [regions_on_and_off_agree_programs_0],
    1 => [regions_on_and_off_agree_programs_1],
    2 => [
        regions_on_and_off_agree_programs_2,
        regions_on_and_off_agree_programs_2_sweep_1,
        regions_on_and_off_agree_programs_2_sweep_2,
        regions_on_and_off_agree_programs_2_sweep_3
    ],
    3 => [regions_on_and_off_agree_programs_3],
    4 => [regions_on_and_off_agree_programs_4],
    5 => [
        regions_on_and_off_agree_programs_5,
        regions_on_and_off_agree_programs_5_sweep_1,
        regions_on_and_off_agree_programs_5_sweep_2,
        regions_on_and_off_agree_programs_5_sweep_3
    ],
    6 => [
        regions_on_and_off_agree_programs_6,
        regions_on_and_off_agree_programs_6_sweep_1,
        regions_on_and_off_agree_programs_6_sweep_2,
        regions_on_and_off_agree_programs_6_sweep_3
    ],
    7 => [regions_on_and_off_agree_programs_7],
    8 => [regions_on_and_off_agree_programs_8],
    9 => [regions_on_and_off_agree_programs_9],
    10 => [regions_on_and_off_agree_programs_10],
    11 => [regions_on_and_off_agree_programs_11],
    12 => [regions_on_and_off_agree_programs_12],
    13 => [regions_on_and_off_agree_programs_13],
    14 => [
        regions_on_and_off_agree_programs_14,
        regions_on_and_off_agree_programs_14_sweep_1,
        regions_on_and_off_agree_programs_14_sweep_2,
        regions_on_and_off_agree_programs_14_sweep_3
    ],
    15 => [regions_on_and_off_agree_programs_15];
    extra = [
        regions_on_and_off_agree_programs_16,
        regions_on_and_off_agree_programs_17,
        regions_on_and_off_agree_programs_18,
        regions_on_and_off_agree_programs_19,
        regions_on_and_off_agree_programs_20,
        regions_on_and_off_agree_programs_21,
        regions_on_and_off_agree_programs_22,
        regions_on_and_off_agree_programs_23,
        regions_on_and_off_agree_programs_24,
        regions_on_and_off_agree_programs_25,
        regions_on_and_off_agree_programs_26,
        regions_on_and_off_agree_programs_27,
        regions_on_and_off_agree_programs_28,
        regions_on_and_off_agree_programs_29,
        regions_on_and_off_agree_programs_30,
        regions_on_and_off_agree_programs_31,
        regions_on_and_off_agree_programs_32,
        regions_on_and_off_agree_programs_33,
        regions_on_and_off_agree_programs_34,
        regions_on_and_off_agree_programs_35,
        regions_on_and_off_agree_programs_36,
        regions_on_and_off_agree_programs_37,
        regions_on_and_off_agree_programs_38,
        regions_on_and_off_agree_programs_39,
        regions_on_and_off_agree_programs_40,
        regions_on_and_off_agree_programs_41,
        regions_on_and_off_agree_programs_42,
        regions_on_and_off_agree_programs_43,
        regions_on_and_off_agree_programs_44,
        regions_on_and_off_agree_programs_45,
        regions_on_and_off_agree_programs_46,
        regions_on_and_off_agree_programs_47,
        regions_on_and_off_agree_programs_48,
        regions_on_and_off_agree_programs_49,
        regions_on_and_off_agree_programs_50,
        regions_on_and_off_agree_programs_51
    ],
}
