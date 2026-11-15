//! The prepared library: a function registry and its build image, shared by
//! every [`Program`](crate::Program) that runs against them, with the
//! core's compiled library bodies cached per build image.
//!
//! A release image records each library body's census ([`body_census`]),
//! and a frame records the library's reservation index
//! ([`PreparedLibrary::frame_index`]); both are build artifacts whose bytes
//! this module defines.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, Mutex, OnceLock, Weak};

use lash_kernel_doc as doc;
use lash_kernel_doc::{Document, FunctionDefinition, FunctionRegistry};
use lash_kernel_value::names::DocumentId;
use lash_kernel_value::sites::{Site, Unit};

use crate::functions::machine_function;

/// A function registry and its build image, prepared once and handed to
/// every [`Program`](crate::Program) (`crate::Program`). The core compiles
/// each library body it runs once per build image and keeps it here.
///
/// It is a cache: derived deterministically from the registry and never
/// saved. A body is found by its function's identity, which is the
/// function's content, so a parked run that pins a function resumes in the
/// body it parked in (`K-MACH-008`).
#[derive(Clone)]
pub struct PreparedLibrary(Arc<Prepared>, Arc<doc::BuildHelpers>);

struct Prepared {
    charge: crate::Charge,
    alternative: OnceLock<PreparedLibrary>,
    registry: Arc<FunctionRegistry>,
    /// Each live document's identity, by its allocation.
    documents: Mutex<BTreeMap<usize, Arc<ImmutableIdentity>>>,
    /// The digest of the registry the core's compiled library is keyed
    /// by, taken once.
    core: OnceLock<[u8; 32]>,
}

struct ImmutableIdentity {
    document: Weak<Document>,
    identity: OnceLock<DocumentId>,
}

impl fmt::Debug for PreparedLibrary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PreparedLibrary")
            .field("functions", &self.0.registry.len())
            .finish_non_exhaustive()
    }
}

impl PreparedLibrary {
    /// Install the immutable build image independently of ordinary definitions.
    pub fn with_helpers(registry: Arc<FunctionRegistry>, helpers: Arc<doc::BuildHelpers>) -> Self {
        let mut prepared = Self::new(registry);
        prepared.1 = helpers;
        prepared
    }

    pub fn helpers(&self) -> &Arc<doc::BuildHelpers> {
        &self.1
    }

    pub fn with_helper_handle(mut self, helpers: Arc<doc::BuildHelpers>) -> Self {
        self.1 = helpers;
        self
    }

    /// Prepares the registry. Each body compiles once, when first needed.
    pub fn new(registry: Arc<FunctionRegistry>) -> Self {
        Self::with_charge(registry, crate::Charge::Exact)
    }

    /// Prepares the registry for the run's charge policy.
    pub fn with_charge(registry: Arc<FunctionRegistry>, charge: crate::Charge) -> Self {
        Self(
            Arc::new(Prepared {
                charge,
                alternative: OnceLock::new(),
                registry,
                documents: Mutex::default(),
                core: OnceLock::new(),
            }),
            doc::BuildHelpers::empty(),
        )
    }

    /// Selects the run's charge policy while keeping this library's closed
    /// helper image. The alternate preparation is shared by subsequent
    /// selections of the same policy.
    pub fn in_charge(&self, charge: crate::Charge) -> Self {
        if self.0.charge == charge {
            self.clone()
        } else {
            self.0
                .alternative
                .get_or_init(|| Self::with_charge(Arc::clone(&self.0.registry), charge))
                .clone()
                .with_helper_handle(Arc::clone(&self.1))
        }
    }

    /// The registry the library was prepared from.
    pub fn registry(&self) -> &Arc<FunctionRegistry> {
        &self.0.registry
    }

    /// The process's compiled library of this registry under build image
    /// `available` (`None`: the registry's own).
    pub(crate) fn core(
        &self,
        available: Option<&doc::BuildHelpers>,
    ) -> Arc<crate::bytecode::library::SharedLibrary> {
        let digest = self
            .0
            .core
            .get_or_init(|| crate::bytecode::library::digest(&self.0.registry));
        crate::bytecode::library::held(crate::bytecode::library::key(digest, available))
    }

    pub(crate) fn note_identified_document(&self, document: &doc::IdentifiedDocument) {
        let key = Arc::as_ptr(document.shared()) as usize;
        let mut documents = self
            .0
            .documents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        documents.retain(|_, known| known.document.strong_count() != 0);
        documents.entry(key).or_insert_with(|| {
            Arc::new(ImmutableIdentity {
                document: Arc::downgrade(document.shared()),
                identity: OnceLock::from(document.id()),
            })
        });
    }

    pub(crate) fn document_identity(&self, document: &Arc<Document>) -> DocumentId {
        let key = Arc::as_ptr(document) as usize;
        let known = {
            let mut documents = self
                .0
                .documents
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(known) = documents.get(&key) {
                Arc::clone(known)
            } else {
                documents.retain(|_, known| known.document.strong_count() != 0);
                let known = Arc::new(ImmutableIdentity {
                    document: Arc::downgrade(document),
                    identity: OnceLock::new(),
                });
                documents.insert(key, Arc::clone(&known));
                known
            }
        };
        // Canonical encoding runs once for this live immutable allocation,
        // outside the library cache lock. Its result never crosses a mutation.
        *known.identity.get_or_init(|| document.identity())
    }

    /// Every body's reservation in registry order, with the totals first.
    #[must_use]
    pub fn frame_index(registry: &FunctionRegistry) -> Vec<u8> {
        let mut bodies = Vec::new();
        let mut totals = [0u32; 4];
        let mut occurrences = Vec::new();
        for (at, (_, registered)) in registry.iter().enumerate() {
            let Some(census) = census_of(registered, registry) else {
                continue;
            };
            bodies.extend(
                [
                    at as u32,
                    totals[0],
                    totals[1],
                    totals[2],
                    totals[3],
                    census.codes,
                    census.blocks,
                    census.stmts,
                    census.sites,
                    u32::from(census.closures),
                ]
                .iter()
                .flat_map(|word| word.to_le_bytes()),
            );
            totals[0] += census.codes;
            totals[1] += census.blocks;
            totals[2] += census.stmts;
            totals[3] += census.sites;
            for site in &census.occurrences {
                let encoded = lash_kernel_value::tokens::encode_body(site);
                occurrences.extend((encoded.len() as u32).to_le_bytes());
                occurrences.extend(encoded);
            }
        }
        let mut out = Vec::new();
        for word in [
            registry.len() as u32,
            (bodies.len() / 40) as u32,
            totals[0],
            totals[1],
            totals[2],
            totals[3],
            occurrences.len() as u32,
        ] {
            out.extend(word.to_le_bytes());
        }
        out.extend(bodies);
        out.extend(occurrences);
        out
    }

    /// The library over `registry`, its reservations read from `index`
    /// ([`PreparedLibrary::frame_index`] over the same functions).
    ///
    /// # Errors
    ///
    /// An `index` that is not that registry's reservation index.
    pub fn over_frame(
        registry: Arc<FunctionRegistry>,
        charge: crate::Charge,
        index: &'static [u8],
    ) -> std::io::Result<Self> {
        if index.len() < 28 {
            return Err(std::io::Error::other("truncated library reservation"));
        }
        let word = |at: usize| {
            u32::from_le_bytes([index[at], index[at + 1], index[at + 2], index[at + 3]])
        };
        let (n, count) = (word(0) as usize, word(4) as usize);
        if n != registry.len()
            || 28usize
                .checked_add(count.saturating_mul(40))
                .and_then(|size| size.checked_add(word(24) as usize))
                != Some(index.len())
        {
            return Err(std::io::Error::other(
                "library reservation does not match the frame",
            ));
        }
        let mut at = 28 + count * 40;
        let end = at + word(24) as usize;
        while at < end {
            let len = word(at) as usize;
            at += 4;
            let _: Site = lash_kernel_value::tokens::decode_body(
                index
                    .get(at..at + len)
                    .ok_or_else(|| std::io::Error::other("truncated occurrence site"))?,
            )
            .map_err(std::io::Error::other)?;
            at += len;
        }
        Ok(Self::with_charge(registry, charge))
    }
}

/// What preparation reserves for a library function's body before it
/// compiles it ([`doc::BodyCensus`]): none for a function with no body or
/// one the machine runs itself. A build writes it into its release image
/// (FIG-6843), and a process computes it only for a function registered one
/// by one.
pub fn body_census(
    definition: &FunctionDefinition,
    catalog: &dyn doc::FunctionCatalog,
) -> Option<doc::BodyCensus> {
    if machine_function(definition).is_some() {
        return None;
    }
    let body = definition.body()?;
    let mut census = doc::BodyCensus {
        codes: 1,
        ..doc::BodyCensus::default()
    };
    // Each pending node retains its path in its own unit. Visiting a
    // closure's body must not replace the path of a pending parent sibling.
    let unit = Unit::Library(definition.identity());
    let charged = matches!(
        definition.implementation,
        doc::Implementation::Guest {
            metering: doc::Metering::Statements,
            ..
        }
    );
    let mut pending = vec![(
        doc::Node::Block(&body.block),
        Vec::<u32>::new(),
        unit,
        charged,
    )];
    while let Some((node, path, unit, charged)) = pending.pop() {
        // Match the property cache sites lowering allocates, without
        // constructing any executable tables or constants.
        census.sites += match node {
            doc::Node::Expr(doc::Expr::Member(_))
            | doc::Node::Stmt(
                doc::Stmt::Assign {
                    place: doc::Place::Member(doc::Member::Field { .. }),
                    ..
                }
                | doc::Stmt::Remove {
                    member: doc::Member::Field { .. },
                },
            ) => 1,
            _ => 0,
        };
        match node {
            doc::Node::Block(_) => census.blocks += 1,
            doc::Node::Stmt(_) => census.stmts += 1,
            doc::Node::Expr(doc::Expr::Closure(closure)) => {
                census.codes += 1;
                census.closures = true;
                if let Some(definition) = catalog.definition(&closure.function)
                    && let Some(body) = definition.body()
                {
                    pending.push((
                        doc::Node::Block(&body.block),
                        Vec::new(),
                        Unit::Library(closure.function),
                        true,
                    ));
                }
            }
            _ => {}
        }
        let counted = match node {
            doc::Node::Action(action) => {
                charged
                    || matches!(
                        action,
                        doc::Action::Perform { .. } | doc::Action::Spawn { .. }
                    )
            }
            doc::Node::Expr(doc::Expr::Act(act)) => {
                charged
                    || matches!(
                        act.kind,
                        doc::ActKind::Perform { .. } | doc::ActKind::Spawn(_)
                    )
            }
            _ => false,
        };
        if counted {
            census
                .occurrences
                .push(Site::new(unit.clone(), path.clone()));
        }
        for (index, child) in node.children().into_iter().enumerate() {
            let mut child_path = path.clone();
            child_path.push(index as u32);
            pending.push((child, child_path, unit.clone(), charged));
        }
    }
    Some(census)
}

/// A registered function's census: its build's, for a released function,
/// and computed from its definition otherwise.
fn census_of(
    registered: &doc::RegisteredFunction,
    catalog: &dyn doc::FunctionCatalog,
) -> Option<doc::BodyCensus> {
    if registered.released() {
        registered.census()
    } else {
        body_census(registered.definition(), catalog)
    }
}
