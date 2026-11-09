//! A bytecode-core session (cutover contract §6, `K-SES-001`): the
//! bindings a terminal cell hands on, as the roots of a sealed object
//! graph. A binding's value is one of the heap root's values, so two
//! bindings that share an object name it by the same [`ObjectId`] and
//! share it in the next cell (`K-SES-002`).
//!
//! A saved function (`K-SES-004`) is a closure or function object of the
//! same heap, one root past the bindings: a binding that holds it carries
//! it by the same identity, and its code is named by document coordinates
//! (its row's link and callable), never by a unit of the cell's program.
//!
//! [`ObjectId`]: super::ids::ObjectId

use lash_kernel_doc::heap::{HeapFault, PageRef};
use lash_kernel_value::names::Name;
use lash_kernel_value::tokens::{
    TokenCodec, TokenError, TokenReader, TokenWriter, decode_body, encode_body,
};

use super::super::PageKind;
use super::super::tree::Access;
use super::continuation::{read_segmented, retain, write_segmented};
use super::objects::{ObjectKind, RunValue};
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

fn invalid(problem: impl Into<String>) -> TokenError {
    TokenError::Invalid(problem.into())
}

impl TokenCodec for RunSession {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(3);
        self.heap.encode(out);
        self.names.encode(out);
        self.functions.encode(out);
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(3)?;
        let session = Self {
            heap: HeapRoot::decode(input)?,
            names: Vec::decode(input)?,
            functions: Vec::decode(input)?,
        };
        let held = session.names.len() + session.functions.len();
        if held != session.heap.roots.len() {
            return Err(invalid(format!(
                "a session names {held} bindings and functions for {} values",
                session.heap.roots.len()
            )));
        }
        if session.names.windows(2).any(|pair| pair[0] >= pair[1]) {
            return Err(invalid("a session's names are not strictly ascending"));
        }
        if session
            .functions
            .windows(2)
            .any(|pair| pair[0].name >= pair[1].name)
        {
            return Err(invalid(
                "a session's functions are not strictly ascending by name",
            ));
        }
        if let Some(value) = session.heap.roots[session.names.len()..]
            .iter()
            .find(|value| {
                !matches!(
                    value,
                    RunValue::Object(ObjectKind::Closure | ObjectKind::Function, _)
                )
            })
        {
            return Err(invalid(format!("a session saves {value:?} as a function")));
        }
        Ok(session)
    }
}

/// Seals `session` under a run session page: its encoded bindings in
/// segments, the root naming them and the directory's root page as its
/// children.
///
/// # Errors
///
/// The page port's faults.
pub fn write(access: &Access<'_>, session: &RunSession) -> Result<PageRef, HeapFault> {
    write_segmented(
        access,
        PageKind::RUN_SESSION,
        &encode_body(session),
        session.heap.directory.page,
    )
}

/// The session sealed under run session page `page`.
///
/// # Errors
///
/// The page port's faults; [`HeapFault::Corrupt`] for a page that is not
/// a run session, a segment that is not one, or bindings that do not
/// decode or name another directory than the root retains.
pub fn read(access: &Access<'_>, page: PageRef) -> Result<RunSession, HeapFault> {
    let (directory, bytes) = read_segmented(access, page, PageKind::RUN_SESSION)?;
    let mut session: RunSession = decode_body(&bytes).map_err(|error| HeapFault::Corrupt {
        page: page.id,
        reason: error.to_string(),
    })?;
    retain(page, &mut session.heap, directory)?;
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
