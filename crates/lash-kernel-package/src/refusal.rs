//! Why a package, a set or an install is refused: one typed enum that
//! crosses the pool wire intact.

use lash_kernel_dialect::{MemberPath, PackagePath, Segment};
use lash_kernel_doc::{ArtifactDigest, EffectName, FunctionId, FunctionName, Signature};
use serde::{Deserialize, Serialize};

use crate::ArtifactKey;

/// What a declaration declares, for a refusal naming two of them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Declared {
    Operation,
    Function,
    Type,
    Error,
    Static,
    Method,
    Property,
}

impl std::fmt::Display for Declared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Operation => "an operation",
            Self::Function => "a function",
            Self::Type => "a type",
            Self::Error => "an error",
            Self::Static => "a static",
            Self::Method => "a method",
            Self::Property => "a property",
        })
    }
}

/// Which name a non-canonical segment was written as.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NameRole {
    Path,
    Export,
    Member,
}

impl std::fmt::Display for NameRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Path => "package path segment",
            Self::Export => "export",
            Self::Member => "type member",
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(rename_all = "snake_case")]
pub enum PackageRefusal {
    /// PKG-NAMES: a path, export or member segment is not canonical.
    #[error(
        "the {role} `{text}` is not canonical: write lowercase words joined by `_`, each starting with a letter"
    )]
    NotCanonical { role: NameRole, text: String },
    /// EXPORT-ONE: a segment is declared twice in one package's exports or
    /// one type's members.
    #[error("`{segment}` is declared twice in `{scope}`: {first} and {second}")]
    Repeated {
        scope: String,
        segment: Segment,
        first: Declared,
        second: Declared,
    },
    /// PKG-ROOTS: a definition sits outside every root its package lists.
    #[error("`{definition}` sits outside the roots of package `{package}`")]
    OutsideRoots {
        package: PackagePath,
        definition: FunctionName,
    },
    /// PKG-ROOTS: a host package's path meets a root a lash package lists.
    #[error(
        "package path `{package}` is reserved: lash's package `{owner}` lists the root `{root}`"
    )]
    ReservedRoot {
        package: PackagePath,
        root: String,
        owner: PackagePath,
    },
    /// PKG-SURFACE: a member names a definition the package does not hold.
    #[error(
        "`{package}`'s member `{member}` names `{definition}`, which the package does not define"
    )]
    NoDefinition {
        package: PackagePath,
        member: MemberPath,
        definition: FunctionName,
    },
    /// PKG-SURFACE: a member's slots are not its definition's.
    #[error(
        "`{package}`'s member `{member}` declares other slots than its definition `{definition}`"
    )]
    OtherSlots {
        package: PackagePath,
        member: MemberPath,
        definition: FunctionName,
        member_signature: Box<Signature>,
        definition_signature: Box<Signature>,
    },
    /// A member declared without `async` whose definition can wait.
    #[error(
        "`{package}`'s member `{member}` is not `async`, and its definition `{definition}` can wait"
    )]
    SyncMemberWaits {
        package: PackagePath,
        member: MemberPath,
        definition: FunctionName,
    },
    /// PKG-OPS: a definition performs an operation its package does not
    /// export.
    #[error("`{definition}` performs `{effect}`, which package `{package}` does not export")]
    UndeclaredOperation {
        package: PackagePath,
        definition: FunctionName,
        effect: EffectName,
    },
    /// An operation is declared with a name it cannot have.
    #[error("operation `{effect}` is not an operation of package `{package}`")]
    ForeignOperation {
        package: PackagePath,
        effect: EffectName,
    },
    /// PKG-SET-CLOSED: an artifact requires another artifact of a line than
    /// the set's.
    #[error("{artifact} requires {requires}, and the set holds {held}")]
    NotClosed {
        artifact: Box<ArtifactKey>,
        requires: Box<ArtifactKey>,
        held: SetHolds,
    },
    /// Two artifacts of one line offered for one set.
    #[error("a set holds one artifact of `{path}`, and was given {first} and {second}")]
    TwoOfLine {
        path: PackagePath,
        first: ArtifactDigest,
        second: ArtifactDigest,
    },
    /// A set or a node names an artifact it is not given.
    #[error("{key} is not held")]
    ArtifactNotHeld { key: ArtifactKey },
    /// A node is given an artifact its set does not name: a node is built
    /// with one set and nothing beside it.
    #[error("{key} is not a member of the node's set")]
    NotMember { key: ArtifactKey },
    /// The package's sources do not read.
    #[error("package `{package}`: {message}")]
    Source {
        package: PackagePath,
        message: String,
    },
    /// The registry refused a definition, rendered.
    #[error("package `{package}`: {message}")]
    Registry {
        package: PackagePath,
        message: String,
    },
    /// A definition a stored surface names, read back from the store, that
    /// the node's registry refuses (FIG-6991).
    #[error("a stored package definition is refused: {message}")]
    StoredDefinition { message: String },
    /// The held code could not be redeclared for a successor kernel
    /// version the build interprets: a defect of the build.
    #[error("the held code does not redeclare for a successor kernel version: {message}")]
    Interpretation { message: String },
    /// A `Native`-only definition with no native: refused at install,
    /// never mid-run.
    #[error("`{name}` ({function}) has only a native implementation, and none was given")]
    NativeMissing {
        function: FunctionId,
        name: FunctionName,
    },
    /// A native was given for a function no held artifact defines, or one
    /// that states no native.
    #[error("a native was given for {function}, which no held artifact defines with a native")]
    NativeUnused { function: FunctionId },
}

/// What a set holds of a line an artifact requires.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SetHolds {
    Nothing,
    Other(ArtifactDigest),
}

impl std::fmt::Display for SetHolds {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Nothing => f.write_str("no artifact of that line"),
            Self::Other(digest) => write!(f, "{}", digest.short()),
        }
    }
}
