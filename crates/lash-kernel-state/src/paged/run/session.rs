//! A bytecode-core session (cutover contract §6, `K-SES-001`): the
//! bindings a terminal cell hands on, as the roots of a sealed object
//! graph. A binding's value is one of the heap root's values, so two
//! bindings that share an object name it by the same [`ObjectId`] and
//! share it in the next cell (`K-SES-002`).
//!
//! The session page names the heap's directory and one keyed tree of
//! rows, a binding's or a saved function's, each keyed by its kind and
//! the SHA-256 of its name, so a rebind rewrites its own row's path and
//! what a cell commits does not grow with the session. A name, or a
//! function's description, longer than one payload leaf is a payload tree
//! of its own.
//!
//! A saved function (`K-SES-004`) is a closure or function object of the
//! same heap, one root past the bindings: a binding that holds it carries
//! it by the same identity, and its code is named by document coordinates
//! (its row's link and callable), never by a unit of the cell's program.
//!
//! [`ObjectId`]: super::ids::ObjectId

use std::collections::BTreeMap;

use lash_kernel_doc::heap::{HeapFault, LogicalBytes, PAYLOAD_LEAF_BYTES, PageRef};
use lash_kernel_doc::heap_values::sha256;
use lash_kernel_value::names::Name;
use lash_kernel_value::tokens::{
    TokenCodec, TokenError, TokenReader, TokenWriter, decode_body, encode_body,
};

use super::super::PageKind;
use super::super::tree::{Access, Cell, Tree, grant};
use super::directory::{DirectoryRoot, whole};
use super::ids::HighWater;
use super::objects::{Heads, ObjectKind, Paged, RunValue};
use super::root::HeapRoot;

/// A carried session: each binding's name, in name order, and its value,
/// the heap root's value at the same index; then each saved function, in
/// name order, its object the root past the bindings at its index.
#[derive(Clone, Debug, PartialEq)]
pub struct RunSession {
    pub heap: HeapRoot,
    /// Strictly ascending, one for each of the heap's first roots.
    pub names: Vec<Name>,
    /// Strictly ascending by name, one for each root past the bindings.
    pub functions: Vec<RunFunction>,
}

/// A function the session saved (`K-SES-004`): its declared name and the
/// description its dialect wrote. Its object is a heap root
/// ([`RunSession::function`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunFunction {
    pub name: Name,
    pub description: Vec<u8>,
}

impl RunSession {
    /// The value bound to the binding at `index`.
    pub fn binding(&self, index: usize) -> Option<&RunValue> {
        self.heap.roots.get(..self.names.len())?.get(index)
    }

    /// The closure or function object of the saved function at `index`.
    pub fn function(&self, index: usize) -> Option<&RunValue> {
        self.functions.get(index)?;
        self.heap.roots.get(self.names.len() + index)
    }

    /// Checks the session's own rules: one name per held value, names and
    /// functions strictly ascending, every saved function a closure or
    /// function object.
    fn check(&self) -> Result<(), String> {
        let held = self.names.len() + self.functions.len();
        if held != self.heap.roots.len() {
            return Err(format!(
                "a session names {held} bindings and functions for {} values",
                self.heap.roots.len()
            ));
        }
        if self.names.windows(2).any(|pair| pair[0] >= pair[1]) {
            return Err("a session's names are not strictly ascending".into());
        }
        if self
            .functions
            .windows(2)
            .any(|pair| pair[0].name >= pair[1].name)
        {
            return Err("a session's functions are not strictly ascending by name".into());
        }
        if let Some(value) = self.heap.roots[self.names.len()..].iter().find(|value| {
            !matches!(
                value,
                RunValue::Object(ObjectKind::Closure | ObjectKind::Function, _)
            )
        }) {
            return Err(format!("a session saves {value:?} as a function"));
        }
        Ok(())
    }
}

impl TokenCodec for RunFunction {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(2);
        self.name.encode(out);
        out.bytes(&self.description);
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(2)?;
        Ok(Self {
            name: Name::decode(input)?,
            description: input.bytes()?.to_vec(),
        })
    }
}

/// A row's kind, the first byte of its key.
const BINDING: u8 = 0;
const FUNCTION: u8 = 1;

/// The key of the row of `kind` named `name`.
fn row_key(kind: u8, name: &str) -> Vec<u8> {
    let mut key = Vec::with_capacity(33);
    key.push(kind);
    key.extend_from_slice(&sha256(&[name.as_bytes()]));
    key
}

/// Bytes a row holds: inline up to one payload leaf, a payload tree
/// above.
#[derive(Clone, Debug, PartialEq)]
enum Held {
    Inline(Vec<u8>),
    Paged(Paged),
}

impl Held {
    fn write(access: &Access<'_>, bytes: &[u8], text: bool) -> Result<Self, HeapFault> {
        Ok(if bytes.len() as u64 <= PAYLOAD_LEAF_BYTES {
            Self::Inline(bytes.to_vec())
        } else {
            Self::Paged(Paged::write(access, bytes, text)?)
        })
    }

    fn read(&self, access: &Access<'_>) -> Result<Vec<u8>, HeapFault> {
        match self {
            Self::Inline(bytes) => Ok(bytes.clone()),
            Self::Paged(paged) => paged.read(access),
        }
    }
}

impl TokenCodec for Held {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(2);
        match self {
            Self::Inline(bytes) => {
                out.unsigned(0);
                out.bytes(bytes);
            }
            Self::Paged(paged) => {
                out.unsigned(1);
                paged.encode(out);
            }
        }
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(2)?;
        match input.unsigned()? {
            0 => {
                let bytes = input.bytes()?;
                if bytes.len() as u64 > PAYLOAD_LEAF_BYTES {
                    return Err(invalid("inline session bytes past one payload leaf"));
                }
                Ok(Self::Inline(bytes.to_vec()))
            }
            1 => Ok(Self::Paged(Paged::decode(input)?)),
            other => Err(invalid(format!("session bytes of form {other}"))),
        }
    }
}

/// One row of a session's tree: a binding's or a saved function's name,
/// its value with the head of the immutable it names, and a function's
/// description.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionRow {
    name: Held,
    value: RunValue,
    /// Exactly the head of the immutable [`Self::value`] names.
    heads: Heads,
    /// A saved function's, and only a saved function's.
    description: Option<Held>,
}

impl TokenCodec for SessionRow {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(4);
        self.name.encode(out);
        self.value.encode(out);
        self.heads.encode(out);
        self.description.encode(out);
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(4)?;
        let row = Self {
            name: Held::decode(input)?,
            value: RunValue::decode(input)?,
            heads: Heads::decode(input)?,
            description: Option::decode(input)?,
        };
        row.heads
            .check(std::iter::once(&row.value))
            .map_err(TokenError::Invalid)?;
        Ok(row)
    }
}

impl Cell for SessionRow {
    fn cost(&self) -> u64 {
        0
    }
    fn children(&self, out: &mut Vec<PageRef>) {
        for held in std::iter::once(&self.name).chain(&self.description) {
            if let Held::Paged(paged) = held {
                out.push(paged.page);
            }
        }
    }
    fn grants(&mut self, children: &[PageRef]) -> Result<(), HeapFault> {
        for held in std::iter::once(&mut self.name).chain(&mut self.description) {
            if let Held::Paged(paged) = held {
                grant(&mut paged.page, children)?;
            }
        }
        Ok(())
    }
}

/// A session page: the heap's minted ids and directory, and the rows'
/// tree.
#[derive(Clone, Copy, Debug, PartialEq)]
struct SessionPage {
    high_water: HighWater,
    directory: DirectoryRoot,
    rows: DirectoryRoot,
}

impl TokenCodec for SessionPage {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(3);
        self.high_water.encode(out);
        self.directory.encode(out);
        self.rows.encode(out);
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(3)?;
        Ok(Self {
            high_water: HighWater::decode(input)?,
            directory: DirectoryRoot::decode(input)?,
            rows: DirectoryRoot::decode(input)?,
        })
    }
}

fn invalid(problem: impl Into<String>) -> TokenError {
    TokenError::Invalid(problem.into())
}

/// The session page `page`, its references granted.
fn page_of(access: &Access<'_>, page: PageRef) -> Result<SessionPage, HeapFault> {
    let corrupt = |reason: String| HeapFault::Corrupt {
        page: page.id,
        reason,
    };
    let answer = access.read(page)?;
    if answer.kind != PageKind::RUN_SESSION {
        return Err(corrupt("not a run session page".into()));
    }
    let mut root: SessionPage =
        decode_body(&answer.tokens).map_err(|error| corrupt(error.to_string()))?;
    for held in [&mut root.directory.page, &mut root.rows.page]
        .into_iter()
        .flatten()
    {
        grant(held, &answer.children)?;
    }
    Ok(root)
}

/// The rows' tree `root` names.
fn rows(root: DirectoryRoot) -> Tree<SessionRow> {
    Tree::open(root.page, root.rows, LogicalBytes(0), false)
}

/// The head of the immutable `value` names, from `heads`.
fn heads_of(value: &RunValue, heads: &Heads) -> Result<Heads, HeapFault> {
    match value {
        RunValue::Object(kind, id) if !kind.mutable() => {
            let head = heads.get(*id).ok_or_else(|| {
                HeapFault::Malformed(format!(
                    "a session value names object {} without its head",
                    id.0
                ))
            })?;
            Ok(Heads(vec![(*id, head.clone())]))
        }
        _ => Ok(Heads::default()),
    }
}

/// Seals `session` under a run session page, as an edit of the rows of
/// the session sealed under `prior`: a row whose name, value and
/// description are unchanged keeps its page, so only the changed rows'
/// paths are written.
///
/// # Errors
///
/// The page port's faults; [`HeapFault::Malformed`] for a session that
/// breaks its own rules.
pub fn write(
    access: &Access<'_>,
    session: &RunSession,
    prior: Option<PageRef>,
) -> Result<PageRef, HeapFault> {
    session.check().map_err(HeapFault::Malformed)?;
    let mut tree = match prior {
        None => Tree::new(false),
        Some(page) => rows(page_of(access, page)?.rows),
    };
    let mut old: BTreeMap<Vec<u8>, SessionRow> = BTreeMap::new();
    tree.visit(access, &mut |key, row| {
        old.insert(key.to_vec(), row.clone());
        true
    })?;
    let wanted = session
        .names
        .iter()
        .map(|name| (BINDING, name, None))
        .chain(
            session
                .functions
                .iter()
                .map(|function| (FUNCTION, &function.name, Some(&function.description))),
        );
    for ((kind, name, description), value) in wanted.zip(&session.heap.roots) {
        let key = row_key(kind, name.as_str());
        let heads = heads_of(value, &session.heap.heads)?;
        if let Some(row) = old.remove(&key)
            && row.value == *value
            && row.heads == heads
            && match (&row.description, description) {
                (None, None) => true,
                (Some(held), Some(bytes)) => held.read(access)? == **bytes,
                _ => false,
            }
        {
            continue;
        }
        let row = SessionRow {
            name: Held::write(access, name.as_str().as_bytes(), true)?,
            value: value.clone(),
            heads,
            description: description
                .map(|bytes| Held::write(access, bytes, false))
                .transpose()?,
        };
        tree.write(key, row, access)?;
    }
    for key in old.keys() {
        tree.remove(key, access)?;
    }
    tree.seal(access, &mut whole())?;
    let page = SessionPage {
        high_water: session.heap.high_water,
        directory: session.heap.directory,
        rows: DirectoryRoot {
            page: tree.page(),
            rows: tree.len(),
        },
    };
    let children = page
        .directory
        .page
        .iter()
        .chain(&page.rows.page)
        .copied()
        .collect();
    access.seal(PageKind::RUN_SESSION, encode_body(&page), children)
}

/// The session sealed under run session page `page`.
///
/// # Errors
///
/// The page port's faults; [`HeapFault::Corrupt`] for a page that is not
/// a run session, or rows that do not decode, are keyed by another name
/// or kind, or break the session's rules.
pub fn read(access: &Access<'_>, page: PageRef) -> Result<RunSession, HeapFault> {
    let corrupt = |reason: String| HeapFault::Corrupt {
        page: page.id,
        reason,
    };
    let root = page_of(access, page)?;
    let mut found = Vec::new();
    rows(root.rows).visit(access, &mut |key, row| {
        found.push((key.to_vec(), row.clone()));
        true
    })?;
    let mut bindings = Vec::new();
    let mut functions = Vec::new();
    let mut heads = BTreeMap::new();
    for (key, row) in found {
        let name = String::from_utf8(row.name.read(access)?)
            .map_err(|error| corrupt(error.to_string()))?;
        let kind = match &row.description {
            None => BINDING,
            Some(_) => FUNCTION,
        };
        if key != row_key(kind, &name) {
            return Err(corrupt(format!(
                "a session row is not keyed by its name {name:?}"
            )));
        }
        let name = Name::new(name);
        for (id, head) in row.heads.0 {
            if heads
                .insert(id, head.clone())
                .is_some_and(|held| held != head)
            {
                return Err(corrupt(format!("object {} has two heads", id.0)));
            }
        }
        match row.description {
            None => bindings.push((name, row.value)),
            Some(description) => functions.push((
                RunFunction {
                    name,
                    description: description.read(access)?,
                },
                row.value,
            )),
        }
    }
    bindings.sort_by(|left, right| left.0.cmp(&right.0));
    functions.sort_by(|left, right| left.0.name.cmp(&right.0.name));
    let (names, mut roots): (Vec<Name>, Vec<RunValue>) = bindings.into_iter().unzip();
    let (functions, saved): (Vec<RunFunction>, Vec<RunValue>) = functions.into_iter().unzip();
    roots.extend(saved);
    let session = RunSession {
        heap: HeapRoot {
            high_water: root.high_water,
            directory: root.directory,
            roots,
            heads: Heads(heads.into_iter().collect()),
        },
        names,
        functions,
    };
    session.check().map_err(&corrupt)?;
    session
        .heap
        .heads
        .check(&session.heap.roots)
        .map_err(&corrupt)?;
    if let Some(id) = session
        .heap
        .roots
        .iter()
        .filter_map(RunValue::id)
        .find(|id| !session.heap.high_water.minted(*id))
    {
        return Err(corrupt(format!(
            "a session value names object {}, past the minted ids",
            id.0
        )));
    }
    Ok(session)
}

/// The session sealed under run session page `page`, read through
/// `source`, and its heap's directory over the same port.
///
/// # Errors
///
/// As [`read`].
pub fn open(
    source: Box<dyn super::super::PageSource>,
    page: PageRef,
) -> Result<(RunSession, super::directory::Directory), HeapFault> {
    let access = Access::new(source);
    let session = read(&access, page)?;
    let directory = super::directory::Directory::over(session.heap.directory, access);
    Ok((session, directory))
}
