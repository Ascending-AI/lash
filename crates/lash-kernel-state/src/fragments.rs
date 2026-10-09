//! The stored form of a parked run: a header and one fragment per root,
//! of which a save rewrites only those that changed.
//!
//! * The header is the run's pins and meters and the list of its roots. It
//!   changes with every statement, so it is always written.
//! * A fragment is one root's state and the heap objects that root owns:
//!   each live object exactly once, in the fragment of the first root, in
//!   the header's order, whose walk reaches it. A reference to an object
//!   another root owns is an identity like any other.
//!
//! A fragment is rewritten when its root's state differs from the
//! baseline's, when the set of objects it owns differs, or when one of
//! those objects was written since. Change is read from the heap's write
//! stamps, not found by encoding: a write through any alias marks the
//! object, whichever root owns it.
//!
//! A reader rebuilds the run from the header and every fragment and then
//! writes it again: the header and every fragment must come back byte for
//! byte, so bytes whose partition, order or encoding this writer would not
//! have produced are refused, not normalised.

use std::collections::{BTreeMap, HashSet};

use lash_kernel_doc::{Identity, KERNEL_VERSION, Name, Object, ObjectId, TaskId, Value};
use serde::Deserialize;
use serde::de::DeserializeOwned;

use crate::schema::{
    Bound, Fragment, Header, Incoming, Owned, ParkedCall, ParkedRun, ParkedTask, Root, RootState,
    Run, TaskState,
};

/// The deepest array-and-object nesting a stored part may have. A value
/// nests 128 deep (`K-VAL-034`), two JSON levels to each of its own, under
/// a fixed number of enclosing shapes.
const MAX_JSON_DEPTH: usize = 512;

/// The heap a save reads objects from.
pub trait Objects {
    /// The object under `id`, as data.
    fn object(&self, id: ObjectId) -> Option<Object>;
    /// Adds the objects the object under `id` names. Returns whether the
    /// object exists.
    fn references(&self, id: ObjectId, out: &mut Vec<ObjectId>) -> bool;
    /// A number that changes whenever the object under `id` is written.
    fn written(&self, id: ObjectId) -> u64;
}

impl Objects for BTreeMap<ObjectId, Object> {
    fn object(&self, id: ObjectId) -> Option<Object> {
        self.get(&id).cloned()
    }

    fn references(&self, id: ObjectId, out: &mut Vec<ObjectId>) -> bool {
        let Some(object) = self.get(&id) else {
            return false;
        };
        match object {
            Object::List(items) | Object::Set(items) => {
                items.iter().for_each(|item| value_objects(item, out));
            }
            Object::Map(entries) => {
                for (key, value) in entries {
                    value_objects(key, out);
                    value_objects(value, out);
                }
            }
            Object::Record(fields) => {
                fields
                    .iter()
                    .for_each(|(_, value)| value_objects(value, out));
            }
            Object::Closure(closure) => out.extend(closure.captures.iter().map(|(_, cell)| *cell)),
            Object::Variable(value) => value_objects(value, out),
        }
        true
    }

    fn written(&self, _: ObjectId) -> u64 {
        0
    }
}

/// Adds the heap objects a value names, through tuples, errors and refs.
/// A task handle names a root of its own, not an object.
fn value_objects(value: &Value, out: &mut Vec<ObjectId>) {
    match value {
        Value::Tuple(members) => members.iter().for_each(|member| value_objects(member, out)),
        Value::Error(error) => value_objects(&error.data, out),
        Value::Ref(Identity::Object(id)) => out.push(*id),
        other => out.extend(other.object()),
    }
}

fn root_objects(state: &RootState, out: &mut Vec<ObjectId>) {
    match state {
        RootState::Session(value) => value_objects(value, out),
        RootState::Task(task) => match &task.state {
            TaskState::Resuming(Incoming::Value(value) | Incoming::Raise(value)) => {
                value_objects(value, out);
            }
            TaskState::Ended(
                crate::schema::Ended::Returned(value) | crate::schema::Ended::Raised(value),
            ) => value_objects(value, out),
            _ => {}
        },
        RootState::Call(call) => {
            for binding in &call.bindings {
                match &binding.value {
                    Bound::Value(value) => value_objects(value, out),
                    Bound::Shared(cell) => out.push(*cell),
                }
            }
        }
        RootState::Held(held) => {
            let arguments = held.arguments.iter().flatten();
            for value in held.iterated.iter().chain(&held.departing).chain(arguments) {
                value_objects(value, out);
            }
        }
    }
}

/// What a save wrote, reduced to what the next save compares against.
///
/// It stands for a set of stored fragments: the caller keeps the two
/// together. Saving against the default, empty baseline writes every
/// fragment. A baseline belongs to the heap it was taken over: the one a
/// save returns to the machine that saved, the one [`ParkedRun::load`]
/// returns to the machine that imports what was loaded.
#[derive(Clone, Debug, Default)]
pub struct Baseline {
    roots: BTreeMap<Root, Fingerprint>,
}

#[derive(Clone, Debug)]
struct Fingerprint {
    state: RootState,
    /// The owned objects, ascending by identity, each with its write
    /// stamp.
    objects: Vec<(ObjectId, u64)>,
}

/// One root's fragment in a save.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SavedFragment {
    /// The fragment is the one the baseline stands for: its stored bytes
    /// still hold.
    Unchanged,
    /// The fragment's bytes.
    Changed(Vec<u8>),
}

/// A save: the header, every root's fragment, and the change set against
/// the baseline it was taken from.
#[derive(Clone, Debug)]
pub struct Saved {
    pub header: Vec<u8>,
    /// Every root the run has.
    pub fragments: BTreeMap<Root, SavedFragment>,
    /// The roots the baseline had and the run no longer has: their stored
    /// fragments are deleted.
    pub removed: Vec<Root>,
    /// What this save wrote, for the next one to compare against.
    pub baseline: Baseline,
}

impl Saved {
    /// The fragments to write: exactly those whose content changed.
    pub fn changed(&self) -> impl Iterator<Item = (&Root, &[u8])> {
        self.fragments
            .iter()
            .filter_map(|(root, fragment)| match fragment {
                SavedFragment::Changed(bytes) => Some((root, bytes.as_slice())),
                SavedFragment::Unchanged => None,
            })
    }
}

/// A run that cannot be written.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum SaveError {
    #[error("{root:?} reaches object {}, which the heap does not hold", .object.0)]
    Dangling { root: Root, object: ObjectId },
    #[error("cannot encode {part}: {message}")]
    Encode { part: String, message: String },
}

/// Stored parts that are not a parked run this writer wrote.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum LoadError {
    #[error("the state was parked under kernel version {parked}; this reader reads {supported}")]
    KernelVersion { parked: u32, supported: u32 },
    #[error("{part} nests {depth} deep; the limit is {limit}")]
    TooDeep {
        part: String,
        depth: usize,
        limit: usize,
    },
    #[error("{part} does not decode: {message}")]
    Undecodable { part: String, message: String },
    #[error("the header lists {root:?}, and no fragment was given for it")]
    Missing { root: Root },
    #[error("a fragment was given for {root:?}, which the header does not list")]
    Unlisted { root: Root },
    #[error("{root:?} is out of place: {problem}")]
    Misplaced { root: Root, problem: String },
    #[error("object {} is stored twice", .object.0)]
    Duplicate { object: ObjectId },
    #[error("{part} is not the bytes this writer produces for what it holds")]
    NotCanonical { part: String },
    #[error("the parts do not save again: {0}")]
    Unsaveable(#[from] SaveError),
}

fn roots_of(session: BTreeMap<Name, Value>, tasks: Vec<ParkedTask>) -> Vec<(Root, RootState)> {
    let mut roots = Vec::new();
    for (name, value) in session {
        roots.push((Root::Session(name), RootState::Session(value)));
    }
    for (index, task) in tasks.into_iter().enumerate() {
        let id = TaskId(index as u64);
        roots.push((Root::Task(id), RootState::Task(Box::new(task.handle))));
        for (depth, ParkedCall { call, held }) in task.calls.into_iter().enumerate() {
            let depth = depth as u32;
            roots.push((Root::Call { task: id, depth }, RootState::Call(call)));
            if !held.is_empty() {
                roots.push((Root::Held { task: id, depth }, RootState::Held(held)));
            }
        }
    }
    roots
}

fn encode<T: serde::Serialize>(
    value: &T,
    part: impl FnOnce() -> String,
) -> Result<Vec<u8>, SaveError> {
    serde_json::to_vec(value).map_err(|error| SaveError::Encode {
        part: part(),
        message: error.to_string(),
    })
}

/// Writes a run whose objects live in `objects`, rewriting only the
/// fragments that differ from `since`.
///
/// A machine saves through this without first copying its heap out: the
/// walk that decides ownership reads references and write stamps, and an
/// object becomes data only when its fragment is rewritten.
/// [`ParkedRun::save`] is this over the run's own objects.
pub fn save(
    run: Run,
    session: BTreeMap<Name, Value>,
    tasks: Vec<ParkedTask>,
    objects: &dyn Objects,
    since: &Baseline,
) -> Result<Saved, SaveError> {
    let roots = roots_of(session, tasks);
    let header = Header {
        run,
        roots: roots.iter().map(|(root, _)| root.clone()).collect(),
    };
    let mut owned: HashSet<ObjectId> = HashSet::new();
    let mut fragments = BTreeMap::new();
    let mut baseline = Baseline::default();
    let mut pending = Vec::new();
    for (root, state) in roots {
        // First discovery: the root owns what its walk reaches and no
        // earlier root's did.
        let mut carried = Vec::new();
        root_objects(&state, &mut pending);
        while let Some(id) = pending.pop() {
            if !owned.insert(id) {
                continue;
            }
            if !objects.references(id, &mut pending) {
                return Err(SaveError::Dangling { root, object: id });
            }
            carried.push(id);
        }
        carried.sort_unstable();
        let fingerprint = Fingerprint {
            objects: carried
                .iter()
                .map(|id| (*id, objects.written(*id)))
                .collect(),
            state,
        };
        let unchanged = since.roots.get(&root).is_some_and(|prior| {
            prior.objects == fingerprint.objects && prior.state == fingerprint.state
        });
        let fragment = if unchanged {
            SavedFragment::Unchanged
        } else {
            let mut stored = Vec::with_capacity(carried.len());
            for id in carried {
                let Some(object) = objects.object(id) else {
                    return Err(SaveError::Dangling { root, object: id });
                };
                stored.push(Owned { id, object });
            }
            let fragment = Fragment {
                root: fingerprint.state.clone(),
                objects: stored,
            };
            SavedFragment::Changed(encode(&fragment, || format!("{root:?}"))?)
        };
        fragments.insert(root.clone(), fragment);
        baseline.roots.insert(root, fingerprint);
    }
    let removed = since
        .roots
        .keys()
        .filter(|root| !baseline.roots.contains_key(*root))
        .cloned()
        .collect();
    Ok(Saved {
        header: encode(&header, || "the header".to_string())?,
        fragments,
        removed,
        baseline,
    })
}

/// The deepest nesting of arrays and objects in a JSON text, counted
/// without recursion so that an over-deep text is refused before it is
/// decoded.
fn json_depth(bytes: &[u8]) -> usize {
    let mut depth = 0usize;
    let mut deepest = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for byte in bytes {
        if in_string {
            match byte {
                _ if escaped => escaped = false,
                b'\\' => escaped = true,
                b'"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'{' | b'[' => {
                depth += 1;
                deepest = deepest.max(depth);
            }
            b'}' | b']' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    deepest
}

fn decode<T: DeserializeOwned>(bytes: &[u8], part: impl Fn() -> String) -> Result<T, LoadError> {
    let depth = json_depth(bytes);
    if depth > MAX_JSON_DEPTH {
        return Err(LoadError::TooDeep {
            part: part(),
            depth,
            limit: MAX_JSON_DEPTH,
        });
    }
    let undecodable = |error: serde_json::Error| LoadError::Undecodable {
        part: part(),
        message: error.to_string(),
    };
    let mut decoder = serde_json::Deserializer::from_slice(bytes);
    decoder.disable_recursion_limit();
    let value = T::deserialize(&mut decoder).map_err(undecodable)?;
    decoder.end().map_err(undecodable)?;
    Ok(value)
}

/// The header's kernel version, read before the header is decoded, so
/// that a state of another version is refused as one and not as a shape it
/// happens not to match.
fn probe_kernel(header: &[u8]) -> Result<u32, LoadError> {
    #[derive(Deserialize)]
    struct Probe {
        run: ProbeRun,
    }
    #[derive(Deserialize)]
    struct ProbeRun {
        kernel: u32,
    }
    let probe: Probe = decode(header, || "the header".to_string())?;
    Ok(probe.run.kernel)
}

impl ParkedRun {
    /// Writes the run as a header and one fragment per root, rewriting
    /// only the fragments that differ from `since`.
    pub fn save(&self, since: &Baseline) -> Result<Saved, SaveError> {
        save(
            self.run.clone(),
            self.session.clone(),
            self.tasks.clone(),
            &self.objects,
            since,
        )
    }

    /// Rebuilds a run from its header and every fragment, refusing
    /// anything this writer would not have produced byte for byte. Returns
    /// the run and the baseline the parts stand for, so that the first
    /// save after an import compares against exactly what was read.
    pub fn load<'a>(
        header: &[u8],
        fragments: impl IntoIterator<Item = (&'a Root, &'a [u8])>,
    ) -> Result<(Self, Baseline), LoadError> {
        let parked = probe_kernel(header)?;
        if parked != KERNEL_VERSION {
            return Err(LoadError::KernelVersion {
                parked,
                supported: KERNEL_VERSION,
            });
        }
        let decoded: Header = decode(header, || "the header".to_string())?;
        let given: BTreeMap<&Root, &[u8]> = fragments.into_iter().collect();
        if let Some(root) = given.keys().find(|root| !decoded.roots.contains(root)) {
            return Err(LoadError::Unlisted {
                root: (*root).clone(),
            });
        }
        let mut run = Self {
            run: decoded.run,
            session: BTreeMap::new(),
            tasks: Vec::new(),
            objects: BTreeMap::new(),
        };
        for root in &decoded.roots {
            let Some(bytes) = given.get(root) else {
                return Err(LoadError::Missing { root: root.clone() });
            };
            let fragment: Fragment = decode(bytes, || format!("{root:?}"))?;
            run.place(root, fragment.root)?;
            for Owned { id, object } in fragment.objects {
                if run.objects.insert(id, object).is_some() {
                    return Err(LoadError::Duplicate { object: id });
                }
            }
        }
        // The fixed point: this writer, run over what was read, must give
        // back exactly the bytes it was given. That one comparison refuses
        // a reordered root, an object owned by the wrong root, an object
        // no root reaches and a spelling this encoder does not write.
        let saved = run.save(&Baseline::default())?;
        if saved.header != header {
            return Err(LoadError::NotCanonical {
                part: "the header".to_string(),
            });
        }
        for (root, bytes) in &given {
            match saved.fragments.get(*root) {
                Some(SavedFragment::Changed(written)) if written.as_slice() == *bytes => {}
                _ => {
                    return Err(LoadError::NotCanonical {
                        part: format!("{root:?}"),
                    });
                }
            }
        }
        Ok((run, saved.baseline))
    }

    /// Puts one root's state where the header's order says it goes.
    fn place(&mut self, root: &Root, state: RootState) -> Result<(), LoadError> {
        let misplaced = |problem: &str| LoadError::Misplaced {
            root: root.clone(),
            problem: problem.to_string(),
        };
        match (root, state) {
            (Root::Session(name), RootState::Session(value)) => {
                if !self.tasks.is_empty() {
                    return Err(misplaced("a session binding follows a task"));
                }
                self.session.insert(name.clone(), value);
            }
            (Root::Task(task), RootState::Task(handle)) => {
                if task.0 != self.tasks.len() as u64 {
                    return Err(misplaced("tasks are listed by handle number, from 0"));
                }
                self.tasks.push(ParkedTask {
                    handle: *handle,
                    calls: Vec::new(),
                });
            }
            (Root::Call { task, depth }, RootState::Call(call)) => {
                let newest = task.0 + 1 == self.tasks.len() as u64;
                let Some(last) = self.tasks.last_mut().filter(|_| newest) else {
                    return Err(misplaced("a call follows its own task's handle"));
                };
                if *depth as usize != last.calls.len() {
                    return Err(misplaced("a task's calls are listed from depth 0"));
                }
                last.calls.push(ParkedCall {
                    call,
                    held: Default::default(),
                });
            }
            (Root::Held { task, depth }, RootState::Held(held)) => {
                let newest = task.0 + 1 == self.tasks.len() as u64;
                let call = self.tasks.last_mut().filter(|_| newest).and_then(|last| {
                    let innermost = last.calls.len();
                    last.calls
                        .last_mut()
                        .filter(|_| *depth as usize + 1 == innermost)
                });
                match call {
                    Some(call) if call.held.is_empty() => call.held = held,
                    _ => return Err(misplaced("held values follow their own call")),
                }
            }
            _ => return Err(misplaced("the fragment holds another kind of root")),
        }
        Ok(())
    }
}
