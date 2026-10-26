// One executable law per rule shard, owned with this corpus; a rule
// in the skip file has no standalone law: a surviving law executes it.
#[test]
fn k_chg_003() {
    super::run_rule("K-CHG-003", include_str!("../../protocol/chg/K-CHG-003.json"));
}

#[test]
fn k_eff_011() {
    super::run_rule("K-EFF-011", include_str!("../../protocol/eff/K-EFF-011.json"));
}

#[test]
fn k_ses_001() {
    super::run_rule("K-SES-001", include_str!("../../protocol/ses/K-SES-001.json"));
}

#[test]
fn k_ses_002() {
    super::run_rule("K-SES-002", include_str!("../../protocol/ses/K-SES-002.json"));
}

#[test]
fn k_ses_003() {
    super::run_rule("K-SES-003", include_str!("../../protocol/ses/K-SES-003.json"));
}

#[test]
fn k_site_001() {
    super::run_rule("K-SITE-001", include_str!("../../protocol/site/K-SITE-001.json"));
}

#[test]
fn k_site_002() {
    super::run_rule("K-SITE-002", include_str!("../../protocol/site/K-SITE-002.json"));
}

#[test]
fn k_site_003() {
    super::run_rule("K-SITE-003", include_str!("../../protocol/site/K-SITE-003.json"));
}

#[test]
fn k_ver_003() {
    super::run_rule("K-VER-003", include_str!("../../protocol/ver/K-VER-003.json"));
}
