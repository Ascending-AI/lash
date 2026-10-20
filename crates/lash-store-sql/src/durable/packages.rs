//! Package artifacts and sets, and the artifacts each live node holds
//! (FIG-6918, S1-F2): the neutral statements of the `packages` domain.
//!
//! An artifact row is keyed by the artifact's digest, which its path is
//! among the facts of, and carries the ordinal the store assigned it on its
//! package line, for listings. A set row is keyed by the set's digest and
//! carries the store's install sequence, the only key behind "newest
//! first"; its members are rows of their own. A node's held artifacts are
//! rows of the node, deleted with it. "Node N holds set S" is derived from
//! them and never stored. Each claim statement that decides whether a node
//! runs an actor spells it over the actor's `packages` column, as
//!
//! ```text
//! a.packages IS NULL OR (
//!     EXISTS (SELECT 1 FROM package_sets AS ps WHERE ps.digest = a.packages)
//!     AND NOT EXISTS (SELECT 1 FROM package_set_members AS pm
//!         WHERE pm.set_digest = a.packages
//!           AND NOT EXISTS (SELECT 1 FROM node_artifacts AS nh
//!               WHERE nh.node_id = <node> AND nh.artifact = pm.artifact)))
//! ```
//!
//! (CLAIM-HOLDS): the set is installed and the node holds every member.

/// `node_artifacts`: one row per package artifact a live node holds.
pub const NODE_ARTIFACTS_TABLE: &str = "node_artifacts";

/// `package_artifacts`: one row per installed artifact, with its bytes.
pub const ARTIFACTS_TABLE: &str = "package_artifacts";

/// `package_sets`: one row per installed set, with its install sequence.
pub const SETS_TABLE: &str = "package_sets";

/// `package_set_members`: one row per member artifact of a set.
pub const MEMBERS_TABLE: &str = "package_set_members";

crate::statements! {
    /// Package statements both backends issue verbatim.
    pub struct PackageStatements @ "durable_packages" {
        /// Record that node `?1` holds artifact `?2` of package path `?3`.
        hold = "INSERT INTO node_artifacts (node_id, artifact, path) VALUES (?1, ?2, ?3)";

        /// Every node whose lease is live at `?1`, with its decoded format
        /// sets as a JSON array and each artifact it holds, one row per
        /// artifact (one row with NULL artifact columns when it holds none),
        /// by node and artifact.
        live_nodes = "SELECT n.node_id, n.formats_json, h.artifact, h.path
             FROM nodes AS n
             LEFT JOIN node_artifacts AS h ON h.node_id = n.node_id
             WHERE n.heartbeat_expires_at_ms >= ?1
             ORDER BY n.node_id, h.artifact";

        /// Install artifact `?1` of package path `?2`, with header `?3`, at
        /// the next ordinal of its line; nothing when it is installed.
        artifact_put = "INSERT INTO package_artifacts (artifact, path, ordinal, header)
             SELECT ?1, ?2, COALESCE(MAX(ordinal), 0) + 1, ?3
             FROM package_artifacts WHERE path = ?2
             ON CONFLICT (artifact) DO NOTHING";

        /// Whether artifact `?1` is installed.
        artifact_known = "SELECT 1 FROM package_artifacts WHERE artifact = ?1";

        /// Install set `?1` at the next install sequence; no row when it is
        /// installed.
        set_put = "INSERT INTO package_sets (digest, sequence)
             SELECT ?1, COALESCE(MAX(sequence), 0) + 1 FROM package_sets WHERE TRUE
             ON CONFLICT (digest) DO NOTHING
             RETURNING digest";

        /// Record artifact `?2` as a member of set `?1`.
        member_put = "INSERT INTO package_set_members (set_digest, artifact) VALUES (?1, ?2)";

        /// Every installed artifact, by line and ordinal.
        artifacts = "SELECT artifact, path, ordinal FROM package_artifacts
             ORDER BY path, ordinal";

        /// Every installed set with each of its members and the member's
        /// path, by install sequence and member.
        sets = "SELECT s.digest, s.sequence, m.artifact, a.path
             FROM package_sets AS s
             JOIN package_set_members AS m ON m.set_digest = s.digest
             JOIN package_artifacts AS a ON a.artifact = m.artifact
             ORDER BY s.sequence, m.artifact";
    }
}
