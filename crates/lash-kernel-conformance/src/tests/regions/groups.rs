// PLAN-INDEP program matrix and core-witness law inventory.
// Each case owns its complete sweep in one program law: its natural run,
// admission and plan census are no longer repeated for every slice. Core
// laws retain every smith seed and the exit/session witnesses independently.

macro_rules! program_shards {
    ($($shard:literal => $name:ident),+; extra = [$($extra:ident: $group:literal),+ $(,)?]) => {
        const GROUPS: usize = [$(stringify!($name)),+, $(stringify!($extra)),+].len();
        $(
            #[test]
            fn $name() {
                regions_on_and_off_agree(Some($shard), None);
            }
        )+
        $(
            #[test]
            fn $extra() {
                regions_on_and_off_agree(Some($group), None);
            }
        )+
        mod core {
            use super::*;
            $(
                #[test]
                fn $name() {
                    regions_on_and_off_agree(None, Some($shard));
                }
            )+
        }
    };
}

program_shards! {
    0 => regions_on_and_off_agree_programs_0,
    1 => regions_on_and_off_agree_programs_1,
    2 => regions_on_and_off_agree_programs_2,
    3 => regions_on_and_off_agree_programs_3,
    4 => regions_on_and_off_agree_programs_4,
    5 => regions_on_and_off_agree_programs_5,
    6 => regions_on_and_off_agree_programs_6,
    7 => regions_on_and_off_agree_programs_7,
    8 => regions_on_and_off_agree_programs_8,
    9 => regions_on_and_off_agree_programs_9,
    10 => regions_on_and_off_agree_programs_10,
    11 => regions_on_and_off_agree_programs_11,
    12 => regions_on_and_off_agree_programs_12,
    13 => regions_on_and_off_agree_programs_13,
    14 => regions_on_and_off_agree_programs_14,
    15 => regions_on_and_off_agree_programs_15;
    extra = [
        regions_on_and_off_agree_programs_16: 16,
        regions_on_and_off_agree_programs_17: 17,
        regions_on_and_off_agree_programs_18: 18,
        regions_on_and_off_agree_programs_19: 19,
        regions_on_and_off_agree_programs_20: 20,
        regions_on_and_off_agree_programs_21: 21,
        regions_on_and_off_agree_programs_22: 22,
        regions_on_and_off_agree_programs_23: 23,
        regions_on_and_off_agree_programs_24: 24,
        regions_on_and_off_agree_programs_25: 25,
        regions_on_and_off_agree_programs_26: 26,
        regions_on_and_off_agree_programs_27: 27,
        regions_on_and_off_agree_programs_28: 28,
        regions_on_and_off_agree_programs_29: 29,
        regions_on_and_off_agree_programs_30: 30,
        regions_on_and_off_agree_programs_31: 31,
    ]
}
