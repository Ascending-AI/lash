//! Native tokens resolve by operand facts, then by a unique full identity.
use lash_kernel_dialect::source_types::{Kind, Types};
use lash_kernel_doc as k;
use lash_vocabulary::{FunctionId, FunctionName, ImportSlot};
use std::collections::BTreeMap;

#[derive(Default)]
pub(crate) struct Operators {
    functions: BTreeMap<FunctionId, FunctionName>,
    /// Only identities selected by the same library as authored lowering.
    helpers: BTreeMap<FunctionId, FunctionName>,
    candidates: BTreeMap<&'static str, Vec<FunctionId>>,
}
impl Operators {
    pub(crate) fn new(document: &k::Document, library: &dyn lash_kernel_dialect::Library) -> Self {
        let mut functions = document.function_names();
        functions.extend(
            document
                .library
                .iter()
                .chain(document.requires.natives.iter())
                .filter_map(|id| library.header(id).map(|header| (*id, header.name.clone()))),
        );
        let helpers: BTreeMap<_, _> = functions
            .iter()
            .filter(|(id, name)| library.resolve(name.as_str()) == Some(**id))
            .map(|(id, name)| (*id, name.clone()))
            .collect();
        let mut candidates: BTreeMap<&'static str, Vec<FunctionId>> = BTreeMap::new();
        for (id, name) in &helpers {
            if let Some(token) = spelling(name.as_str()) {
                candidates.entry(token).or_default().push(*id);
            }
        }
        Self {
            functions,
            helpers,
            candidates,
        }
    }
    pub(crate) fn functions(&self) -> &BTreeMap<FunctionId, FunctionName> {
        &self.functions
    }
    pub(crate) fn id(&self, token: &str, args: &[k::Expr], types: &Types) -> Option<FunctionId> {
        let candidates = self.candidates.get(token)?;
        if let [id] = candidates.as_slice() {
            return Some(*id);
        }
        let numeric = args
            .iter()
            .all(|arg| types.expression(arg, &self.functions) == Kind::Number);
        let text = args
            .iter()
            .all(|arg| types.expression(arg, &self.functions) == Kind::Text);
        let candidates: Vec<_> = candidates
            .iter()
            .filter(|id| match self.functions[id].as_str() {
                "add" | "sub" | "mul" | "neg" | "int.add" | "int.sub" | "int.mul" => numeric,
                "text.concat" => text,
                "py.add" | "py.sub" | "py.mul" | "py.neg" => !numeric && !text,
                _ => true,
            })
            .collect();
        match candidates.as_slice() {
            [id] => Some(**id),
            _ => None,
        }
    }
    pub(crate) fn token(
        &self,
        id: &FunctionId,
        args: &[k::Expr],
        types: &Types,
    ) -> Option<&'static str> {
        let token = spelling(self.helpers.get(id)?.as_str())?;
        let arity = if matches!(token, "not" | "neg") { 1 } else { 2 };
        (args.len() == arity && self.id(token, args, types) == Some(*id)).then_some(token)
    }
}
fn spelling(label: &str) -> Option<&'static str> {
    Some(match label {
        "add" | "int.add" | "text.concat" | "py.add" => "+",
        "sub" | "int.sub" | "py.sub" => "-",
        "mul" | "int.mul" | "py.mul" => "*",
        "py.truediv" => "/",
        "py.floordiv" => "//",
        "py.mod" => "%",
        "py.pow" => "**",
        "eq" | "py.equal" => "==",
        "same" | "py.same" => "is",
        "lt" | "int.lt" => "<",
        "le" | "int.le" => "<=",
        "gt" => ">",
        "ge" => ">=",
        "bool.not" => "not",
        "neg" | "py.neg" => "neg",
        _ => return None,
    })
}

/// How an exported library action is spelled when an author could have
/// written it: a builtin call, or a method of its first operand. Each takes
/// the action's atoms positionally, exactly as authored lowering passed
/// them, so the spelling is the node's exact inverse.
pub(crate) enum Authored {
    Builtin(&'static str),
    Method(String),
    /// `target[start:stop:step]`, an absent bound left empty.
    Slice,
    /// `target[index]`, paired with the indexed-read lowering helper.
    Index,
    /// `type(value).__name__`.
    TypeName,
}

/// The builtins whose helper takes its arguments positionally. `float` is
/// absent: `float("nan")` already spells a literal.
const BUILTINS: [(&str, &str); 18] = [
    ("abs", "py.abs"),
    ("all", "py.all"),
    ("any", "py.any"),
    ("bool", "py.truth"),
    ("chr", "py.chr"),
    ("dict", "py.to_dict"),
    ("divmod", "py.divmod"),
    ("int", "py.int"),
    ("len", "py.length"),
    ("list", "py.to_list"),
    ("ord", "py.ord"),
    ("range", "py.range"),
    ("repr", "py.repr"),
    ("reversed", "py.reversed"),
    ("round", "py.round"),
    ("set", "py.to_set"),
    ("sum", "py.sum"),
    ("tuple", "py.to_tuple"),
];

impl Operators {
    /// Helpers lowered as actions retain their exact library identity, even
    /// when source type inference cannot prove native numeric operands.
    pub(crate) fn action_token(
        &self,
        target: &k::FunctionTarget,
        args: &[k::Expr],
    ) -> Option<&'static str> {
        let label = target.label(self)?;
        let token = spelling(&label)?;
        let arity = if matches!(token, "not" | "neg") { 1 } else { 2 };
        (args.len() == arity).then_some(token)
    }

    /// The one function the base document labels `label`.
    pub(crate) fn unique(&self, label: &str) -> Option<FunctionId> {
        let mut ids = self
            .helpers
            .iter()
            .filter(|(_, name)| name.as_str() == label)
            .map(|(id, _)| *id);
        let id = ids.next()?;
        ids.next().is_none().then_some(id)
    }
    /// The `for` protocol of the base document that a native loop over
    /// `view` spells (`for x in e:` for none, `for x in e.items():` for
    /// `items`): the one function labelled each of its hooks.
    pub(crate) fn for_in(&self, view: Option<&str>) -> Option<k::Protocol> {
        let (open, item) = match view {
            None => ("py.for_in.open", None),
            Some("keys") => ("py.for_in.keys", None),
            Some("values") => ("py.for_in.values", Some("py.for_in.value")),
            Some("items") => ("py.for_in.items", Some("py.for_in.item")),
            Some(_) => return None,
        };
        Some(k::Protocol {
            open: self.target(open)?,
            next: if view.is_none() {
                Some(self.target("py.iterator.step")?)
            } else {
                None
            },
            item: match item {
                Some(item) => Some(self.target(item)?),
                None => None,
            },
            check: Some(self.target("py.for_in.check")?),
        })
    }
    /// The view a native `for` spells for a loop through `via`, or none:
    /// `Some(None)` for `for x in e:`, `Some(Some("items"))` for
    /// `for x in e.items():`. Python's `for` is the loop through the base's
    /// `py.for_in` protocol, or, in a base that labels no such protocol, the
    /// kernel's own loop (`via` none).
    pub(crate) fn native_loop(&self, via: Option<&k::Protocol>) -> Option<Option<&'static str>> {
        match (self.for_in(None).is_some(), via) {
            (true, Some(via)) => [None, Some("keys"), Some("values"), Some("items")]
                .into_iter()
                .find(|view| self.for_in(*view).as_ref() == Some(via)),
            (false, None) => Some(None),
            _ => None,
        }
    }
    /// The authored spelling of a library action, if it has one: a label
    /// the document holds once, and for a method a variable receiver.
    pub(crate) fn authored(&self, id: &impl Reference, args: &[k::Expr]) -> Option<Authored> {
        let name = id.label(self)?;
        let label = name.as_str();
        if args.is_empty() {
            return None;
        }
        if let Some((name, _)) = BUILTINS.iter().find(|(_, helper)| *helper == label) {
            return Some(Authored::Builtin(name));
        }
        match label {
            "py.getitem" => (args.len() == 2).then_some(Authored::Index),
            "py.slice" => (args.len() == 4).then_some(Authored::Slice),
            "py.type" => (args.len() == 1).then_some(Authored::TypeName),
            _ => {
                let method = label.strip_prefix("py.method.")?;
                crate::printer::attribute_identifier(method)
                    .then(|| Authored::Method(method.to_owned()))
            }
        }
    }
}

pub(crate) trait Reference {
    fn label(&self, operators: &Operators) -> Option<String>;
    fn target(&self) -> k::FunctionTarget;
}
impl Reference for FunctionId {
    fn label(&self, operators: &Operators) -> Option<String> {
        operators.helpers.get(self).map(ToString::to_string)
    }
    fn target(&self) -> k::FunctionTarget {
        k::FunctionTarget::Local(*self)
    }
}
impl Reference for ImportSlot {
    fn label(&self, _operators: &Operators) -> Option<String> {
        None
    }
    fn target(&self) -> k::FunctionTarget {
        k::FunctionTarget::Import(*self)
    }
}
impl Reference for k::FunctionTarget {
    fn label(&self, operators: &Operators) -> Option<String> {
        match self {
            Self::Local(id) => id.label(operators),
            Self::Import(slot) => slot.label(operators),
        }
    }
    fn target(&self) -> k::FunctionTarget {
        self.clone()
    }
}
impl Operators {
    pub(crate) fn name(&self, reference: &impl Reference) -> Option<String> {
        reference.label(self)
    }
    pub(crate) fn is(&self, reference: &impl Reference, label: &str) -> bool {
        self.target(label).as_ref() == Some(&reference.target())
    }
    pub(crate) fn uses(&self, label: &str) -> bool {
        self.target(label).is_some()
    }
    fn target(&self, label: &str) -> Option<k::FunctionTarget> {
        let mut found = self
            .helpers
            .iter()
            .filter(|(_, name)| name.as_str() == label)
            .map(|(id, _)| k::FunctionTarget::Local(*id));
        let target = found.next()?;
        found.next().is_none().then_some(target)
    }
}
