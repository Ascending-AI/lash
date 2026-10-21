//! What each library body can reach: guest code, task actions, the host
//! (its reads and the effects the body performs) and writes to objects,
//! through the bodies it calls. A helper is closed when it reaches none of
//! the first three. A closed body compiles to a plan that answers at once;
//! one that reaches guest code or performs compiles to a resumable segment.
//! The facts are the per-helper manifest the callback lane and admission
//! read.

use std::collections::{BTreeMap, BTreeSet};

use lash_kernel_doc::ActKind;
use lash_kernel_doc::Action;
use lash_kernel_doc::Callee;
use lash_kernel_doc::Expr;
use lash_kernel_doc::Formula;
use lash_kernel_doc::FunctionDefinition;
use lash_kernel_doc::Node;
use lash_kernel_doc::Operand;
use lash_kernel_doc::Place;
use lash_kernel_doc::Stmt;
use lash_vocabulary::legacy_names::FunctionId;

use crate::library::Library;

/// A body's own facts, before what it calls is taken in.
#[derive(Clone, Debug, Default)]
struct Own {
    /// Where the body applies a value it was given: guest code may run.
    guest: Vec<String>,
    /// Task, scheduling and wait actions.
    tasks: Vec<String>,
    /// Clock, random and projection reads.
    host: Vec<String>,
    /// The `perform`s the body states: it reaches the host there, where a
    /// segment waits as the body's frame does.
    performs: Vec<String>,
    /// Closures the body makes, which run as ordinary charged code.
    closures: usize,
    /// Whether the body writes or removes a member of an object.
    writes: bool,
    /// Whether a loop of the body declares its own charge per test.
    metered: bool,
    /// Whether the body has a `try`.
    handlers: bool,
    callees: BTreeSet<FunctionId>,
}

/// A body's facts with what it calls taken in.
#[derive(Clone, Debug)]
pub struct Facts {
    own: Own,
    /// Whether no guest code, task action or host read is reachable.
    pub closed: bool,
    /// Whether the body or a body it calls writes an object.
    pub mutates: bool,
    /// The callees through which guest code, a task action or a host read
    /// is reachable, by name.
    pub reaching: Vec<String>,
    /// Whether one of them reaches guest code.
    reaching_guest: bool,
    /// Whether the body or one of them performs.
    performing: bool,
}

impl Facts {
    pub fn closures(&self) -> usize {
        self.own.closures
    }

    pub fn metered(&self) -> bool {
        self.own.metered
    }

    pub fn handlers(&self) -> bool {
        self.own.handlers
    }

    pub fn callees(&self) -> &BTreeSet<FunctionId> {
        &self.own.callees
    }

    /// Every site the body itself enters guest code, acts on tasks, reads
    /// the host or performs at, then every callee it reaches one through.
    pub fn callback_sites(&self) -> Vec<String> {
        let own = &self.own;
        own.guest
            .iter()
            .chain(&own.tasks)
            .chain(&own.host)
            .chain(&own.performs)
            .cloned()
            .chain(self.reaching.iter().map(|name| format!("invoke {name}")))
            .collect()
    }

    /// Whether the body compiles to a resumable segment: it is not closed,
    /// and guest code or a `perform` is among what it reaches. The emitter
    /// refuses any other form that leaves the plan.
    pub fn segment(&self) -> bool {
        !self.closed && (self.kind() == "callback" || self.performing)
    }

    /// The manifest's kind: `closed`, or what the body can reach. A
    /// performing body reaches the host.
    pub fn kind(&self) -> &'static str {
        if self.closed {
            "closed"
        } else if !self.own.guest.is_empty() || self.reaching_guest {
            "callback"
        } else if !self.own.tasks.is_empty() {
            "task"
        } else {
            "host"
        }
    }
}

/// The facts of every function with a body in `library`.
pub struct Analysis {
    facts: BTreeMap<FunctionId, Facts>,
}

impl Analysis {
    pub fn of(library: &Library) -> Self {
        let mut analysis = Self {
            facts: BTreeMap::new(),
        };
        for (function, registered) in library.registry.iter() {
            analysis.visit(library, *function, registered.definition());
        }
        analysis
    }

    pub fn facts(&self, function: &FunctionId) -> Option<&Facts> {
        self.facts.get(function)
    }

    fn visit(&mut self, library: &Library, function: FunctionId, definition: &FunctionDefinition) {
        if self.facts.contains_key(&function) {
            return;
        }
        let own = own(definition);
        let mut closed = own.guest.is_empty()
            && own.tasks.is_empty()
            && own.host.is_empty()
            && own.performs.is_empty();
        let mut mutates = own.writes;
        let mut reaching = Vec::new();
        let mut reaching_guest = false;
        let mut performing = !own.performs.is_empty();
        for callee in &own.callees {
            let Some(definition) = library.definition(callee) else {
                // A function the registry lacks reaches what it may.
                closed = false;
                reaching.push(callee.to_string());
                continue;
            };
            if definition.body().is_none() {
                // A native calls no guest code (`K-LIB-006`). The machine's
                // own `tasks.unfinished` reads the task set.
                if definition.name.as_str() == "tasks.unfinished" {
                    closed = false;
                    reaching.push(definition.name.to_string());
                }
                continue;
            }
            self.visit(library, *callee, definition);
            let facts = &self.facts[callee];
            mutates |= facts.mutates;
            if !facts.closed {
                closed = false;
                reaching_guest |= facts.kind() == "callback";
                performing |= facts.performing;
                reaching.push(definition.name.to_string());
            }
        }
        self.facts.insert(
            function,
            Facts {
                own,
                closed,
                mutates,
                reaching,
                reaching_guest,
                performing,
            },
        );
    }
}

/// The facts a body states itself.
fn own(definition: &FunctionDefinition) -> Own {
    let mut own = Own::default();
    let Some(body) = definition.body() else {
        return own;
    };
    let mut pending = vec![Node::Block(&body.block)];
    while let Some(node) = pending.pop() {
        match node {
            Node::Stmt(stmt) => match stmt {
                Stmt::Assign {
                    place: Place::Member(_),
                    ..
                }
                | Stmt::Remove { .. } => own.writes = true,
                Stmt::For { metered, via, .. } => {
                    own.metered |= *metered != 0;
                    if let Some(via) = via {
                        own.callees.extend(via.functions());
                    }
                }
                Stmt::While { metered, .. } => own.metered |= *metered != 0,
                Stmt::ReturnVia { policy, .. } => {
                    own.callees.insert(*policy);
                }
                Stmt::Try(_) => own.handlers = true,
                Stmt::Print { .. } => own.host.push("print".into()),
                Stmt::Finish { .. } | Stmt::Fail { .. } => own.tasks.push("finish".into()),
                _ => {}
            },
            Node::Action(action) => action_facts(action, &mut own),
            Node::Expr(expr) => match expr {
                Expr::Closure(_) => {
                    // Making a closure executes none of its body. Its calls,
                    // writes and host reads happen later as ordinary code.
                    own.closures += 1;
                    continue;
                }
                Expr::Clock => own.host.push("clock".into()),
                Expr::Random => own.host.push("random".into()),
                Expr::Read(_) => own.host.push("read".into()),
                Expr::Call { function, .. } => {
                    own.callees.insert(*function);
                }
                Expr::Act(act) => act_facts(&act.kind, &mut own),
                _ => {}
            },
            Node::Block(_) => {}
        }
        pending.extend(node.children());
    }
    own
}

fn callee_facts(callee: &Callee, own: &mut Own) {
    match callee {
        Callee::Library(function) => {
            own.callees.insert(*function);
        }
        Callee::Value(name) => own.guest.push(format!("apply {name}")),
        Callee::Declared(name) => own.guest.push(format!("call {name}")),
    }
}

fn action_facts(action: &Action, own: &mut Own) {
    match action {
        Action::Call { callee, .. } => callee_facts(callee, own),
        Action::Spawn { callee, .. } => {
            callee_facts(callee, own);
            own.tasks.push("spawn".into());
        }
        Action::Perform { effect, .. } => own.performs.push(format!("perform {effect}")),
        Action::Sleep { .. } => own.tasks.push("sleep".into()),
        Action::Join { .. } | Action::JoinMany { .. } => own.tasks.push("join".into()),
        Action::Yield => own.tasks.push("yield".into()),
        Action::Cancel { .. } => own.tasks.push("cancel".into()),
        Action::Await { policy, .. } => {
            own.callees.insert(*policy);
            own.tasks.push("await".into());
        }
    }
}

fn act_facts(kind: &ActKind, own: &mut Own) {
    match kind {
        ActKind::Call(callee) => callee_facts(callee, own),
        ActKind::Spawn(callee) => {
            callee_facts(callee, own);
            own.tasks.push("spawn".into());
        }
        ActKind::Perform { effect, .. } => own.performs.push(format!("perform {effect}")),
        ActKind::Sleep { .. } => own.tasks.push("sleep".into()),
        ActKind::Join | ActKind::JoinMany(_) => own.tasks.push("join".into()),
        ActKind::Yield => own.tasks.push("yield".into()),
        ActKind::Cancel => own.tasks.push("cancel".into()),
        ActKind::Await(policy) => {
            own.callees.insert(*policy);
            own.tasks.push("await".into());
        }
    }
}

/// A formula as kernel text writes it.
pub fn formula(formula: &Formula) -> String {
    let all = |terms: &[Formula]| {
        terms
            .iter()
            .map(self::formula)
            .collect::<Vec<_>>()
            .join(", ")
    };
    let operand = |operand: &Operand| match operand {
        Operand::Param(name) => name.to_string(),
        Operand::Result => "result".to_string(),
    };
    match formula {
        Formula::Constant(units) => units.to_string(),
        Formula::Size(of) => format!("size({})", operand(of)),
        Formula::DeepSize(of) => format!("deep_size({})", operand(of)),
        Formula::NestedSize(of) => format!("nested_size({})", operand(of)),
        Formula::Magnitude(of) => format!("magnitude({})", operand(of)),
        Formula::Sum(terms) => format!("sum({})", all(terms)),
        Formula::Product(terms) => format!("product({})", all(terms)),
        Formula::Max(terms) => format!("max({})", all(terms)),
        Formula::Min(terms) => format!("min({})", all(terms)),
        Formula::CeilLog2(term) => format!("ceil_log2({})", self::formula(term)),
    }
}
