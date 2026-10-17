//! Native tokens resolve by operand facts, then by a unique full identity.
use lash_kernel_dialect::source_types::{Kind, Types};
use lash_kernel_doc as k;
use std::collections::BTreeMap;

#[derive(Default)]
pub(crate) struct Operators {
    functions: BTreeMap<k::FunctionId, k::FunctionName>,
    candidates: BTreeMap<&'static str, Vec<k::FunctionId>>,
}
impl Operators {
    pub(crate) fn new(functions: &BTreeMap<k::FunctionId, k::FunctionName>) -> Self {
        let mut candidates: BTreeMap<&'static str, Vec<k::FunctionId>> = BTreeMap::new();
        for (id, name) in functions {
            if let Some(token) = spelling(name.as_str()) {
                candidates.entry(token).or_default().push(*id);
            }
        }
        Self {
            functions: functions.clone(),
            candidates,
        }
    }
    pub(crate) fn functions(&self) -> &BTreeMap<k::FunctionId, k::FunctionName> {
        &self.functions
    }
    pub(crate) fn id(&self, token: &str, args: &[k::Expr], types: &Types) -> Option<k::FunctionId> {
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
        id: &k::FunctionId,
        args: &[k::Expr],
        types: &Types,
    ) -> Option<&'static str> {
        let token = spelling(self.functions.get(id)?.as_str())?;
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
        "eq" => "==",
        "same" => "is",
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
    /// `type(value).__name__`.
    TypeName,
}

/// The builtins whose helper takes its arguments positionally. `float` is
/// absent: `float("nan")` already spells a literal.
const BUILTINS: [(&str, &str); 17] = [
    ("abs", "py.abs"),
    ("all", "py.all"),
    ("any", "py.any"),
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
    /// The one function the base document labels `label`.
    pub(crate) fn unique(&self, label: &str) -> Option<k::FunctionId> {
        let mut ids = self
            .functions
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
            open: self.unique(open)?,
            next: None,
            item: match item {
                Some(item) => Some(self.unique(item)?),
                None => None,
            },
            check: Some(self.unique("py.for_in.check")?),
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
    pub(crate) fn authored(&self, id: &k::FunctionId, args: &[k::Expr]) -> Option<Authored> {
        let label = self.functions.get(id)?.as_str();
        if args.is_empty() || self.unique(label) != Some(*id) {
            return None;
        }
        if let Some((name, _)) = BUILTINS.iter().find(|(_, helper)| *helper == label) {
            return Some(Authored::Builtin(name));
        }
        match label {
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

/// The helper a builtin's authored call stands for.
pub(crate) fn builtin_label(name: &str) -> Option<&'static str> {
    BUILTINS
        .iter()
        .find(|(builtin, _)| *builtin == name)
        .map(|(_, helper)| *helper)
}
