//! Package artifacts and sets, and the artifacts each live node holds
//! (FIG-6918, S1-F2): the neutral statements of the `packages` domain.
//!
//! An artifact row is keyed by the artifact's digest, which its path is
//! among the facts of, and carries the ordinal the store assigned it on its
//! package line, for listings. A set row is keyed by the set's digest and
//! carries the store's install sequence, the only key behind "newest
//! first", its surface snapshot and its execution requirements, written
//! once by the install that first stores it (FIG-6991); its members are
//! rows of their own. A node's held artifacts are rows of the node, deleted
//! with it, and its capabilities a column of its row. "Node N holds set S" is derived from
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

/// `package_definitions`: one row per definition of a stored package, by
/// identity and package path, helpers included (FIG-6991). Written only by
/// an install and never deleted: membership is the authority to perform
/// the package's internal operations.
pub const DEFINITIONS_TABLE: &str = "package_definitions";

/// `package_internal_operations`: one row per operation a stored package
/// declares internal, by effect name and the package's path. Written only
/// by an install and never deleted.
pub const INTERNAL_OPERATIONS_TABLE: &str = "package_internal_operations";

/// `package_labels`: one row per distinct labels an install wrote for an
/// artifact, at the store's label sequence. Append-only: a second release
/// of identical content adds its labels beside the first's.
pub const LABELS_TABLE: &str = "package_labels";

crate::statements! {
    /// Package statements both backends issue verbatim.
    pub struct PackageStatements @ "durable_packages" {
        /// Record that node `?1` holds artifact `?2` of package path `?3`.
        hold = "INSERT INTO node_artifacts (node_id, artifact, path) VALUES (?1, ?2, ?3)";

        /// Every node whose lease is live at `?1`, with its decoded format
        /// sets as a JSON array, its capabilities (its own set among them)
        /// as JSON and each artifact
        /// it holds, one row per artifact (one row with NULL artifact
        /// columns when it holds none), by node and artifact.
        live_nodes = "SELECT n.node_id, n.formats_json, n.capabilities_json, h.artifact, h.path
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

        /// Record definition `?1` as one of package `?2`'s; nothing when it
        /// is recorded.
        definition_put = "INSERT INTO package_definitions (function, path) VALUES (?1, ?2)
             ON CONFLICT (function, path) DO NOTHING";

        /// The paths of the stored packages that define `?1`, in order.
        definition_paths = "SELECT path FROM package_definitions WHERE function = ?1
             ORDER BY path";

        /// Record operation `?1` as declared internal by package `?2`;
        /// nothing when it is recorded.
        internal_put = "INSERT INTO package_internal_operations (operation, path) VALUES (?1, ?2)
             ON CONFLICT (operation, path) DO NOTHING";

        /// The paths of the stored packages that declare `?1` internal, in
        /// order.
        internal_paths = "SELECT path FROM package_internal_operations WHERE operation = ?1
             ORDER BY path";

        /// Append labels `?2` of artifact `?1`, keyed by their fixed-width
        /// digest `?3`, at the next label sequence; nothing when recorded.
        label_put = "INSERT INTO package_labels (artifact, labels, labels_digest, sequence)
             SELECT ?1, ?2, ?3, COALESCE(MAX(sequence), 0) + 1 FROM package_labels WHERE TRUE
             ON CONFLICT (artifact, labels_digest) DO NOTHING";

        /// Every labels of artifact `?1`, in install order.
        labels_of = "SELECT labels FROM package_labels WHERE artifact = ?1 ORDER BY sequence";

        /// Whether artifact `?1` is installed.
        artifact_known = "SELECT 1 FROM package_artifacts WHERE artifact = ?1";

        /// Install set `?1` at the next install sequence, with surface
        /// snapshot `?2` and requirements JSON `?3`; no row when it is
        /// installed.
        set_put = "INSERT INTO package_sets (digest, sequence, surface, requirements_json)
             SELECT ?1, COALESCE(MAX(sequence), 0) + 1, ?2, ?3 FROM package_sets WHERE TRUE
             ON CONFLICT (digest) DO NOTHING
             RETURNING digest";

        /// Record artifact `?2` as a member of set `?1`.
        member_put = "INSERT INTO package_set_members (set_digest, artifact) VALUES (?1, ?2)";

        /// Every installed artifact, by line and ordinal.
        artifacts = "SELECT artifact, path, ordinal FROM package_artifacts
             ORDER BY path, ordinal";

        /// Every installed set with its requirements JSON and each of its
        /// members and the member's path, by install sequence and member.
        sets = "SELECT s.digest, s.sequence, s.requirements_json, m.artifact, a.path
             FROM package_sets AS s
             JOIN package_set_members AS m ON m.set_digest = s.digest
             JOIN package_artifacts AS a ON a.artifact = m.artifact
             ORDER BY s.sequence, m.artifact";

        /// The surface snapshot of set `?1`; no row when it is not installed.
        surface = "SELECT surface FROM package_sets WHERE digest = ?1";
    }
}
