//! Statements owned by `usage_owner_retirements`.
pub const TABLE: &str = "usage_owner_retirements";
crate::statements! {
    pub struct UsageOwnerRetirementsStatements @ "usage_owner_retirements" {
        find = "SELECT retired_at_ms FROM usage_owner_retirements WHERE owner_kind = ?1 AND owner_id = ?2";
        delete_retired = "DELETE FROM usage_owner_retirements WHERE retired_at_ms < ?1";
    }
}
