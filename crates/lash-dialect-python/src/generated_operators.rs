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
    /// The authored spelling of a library action, if it has one: a label
    /// the document holds once, and for a method a variable receiver.
    pub(crate) fn authored(&self, id: &k::FunctionId, args: &[k::Atom]) -> Option<Authored> {
        let label = self.functions.get(id)?.as_str();
        if args.is_empty() || self.unique(label) != Some(*id) {
            return None;
        }
        if let Some((name, _)) = BUILTINS.iter().find(|(_, helper)| *helper == label) {
            return Some(Authored::Builtin(name));
        }
        let receiver = matches!(
            args[0],
            k::Atom::Variable(_) | k::Atom::Literal(k::Literal::Text(_))
        );
        match label {
            "py.slice" => (args.len() == 4 && matches!(args[0], k::Atom::Variable(_)))
                .then_some(Authored::Slice),
            "py.type" => (args.len() == 1).then_some(Authored::TypeName),
            _ => {
                let method = label.strip_prefix("py.method.")?;
                (receiver && crate::printer::attribute_identifier(method))
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
