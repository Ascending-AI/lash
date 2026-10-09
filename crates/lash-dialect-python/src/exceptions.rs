//! The exception classes a program may raise and catch.
//!
//! Classes are not values in this dialect. A class name is known when the
//! source is lowered, so `except LookupError` is written out as the kinds
//! it takes: the class and every subclass. An exception is an error value
//! whose kind is its class name (`helpers/core.kernel`).

use std::collections::BTreeMap;

/// The built-in classes with the parent of each, `BaseException` first.
const BUILTIN: &[(&str, &str)] = &[
    ("BaseException", ""),
    ("Exception", "BaseException"),
    ("CancelledError", "BaseException"),
    ("KeyboardInterrupt", "BaseException"),
    ("SystemExit", "BaseException"),
    ("GeneratorExit", "BaseException"),
    ("ArithmeticError", "Exception"),
    ("ZeroDivisionError", "ArithmeticError"),
    ("OverflowError", "ArithmeticError"),
    ("FloatingPointError", "ArithmeticError"),
    ("LookupError", "Exception"),
    ("IndexError", "LookupError"),
    ("KeyError", "LookupError"),
    ("ValueError", "Exception"),
    ("TypeError", "Exception"),
    ("RuntimeError", "Exception"),
    ("RecursionError", "RuntimeError"),
    ("NotImplementedError", "RuntimeError"),
    ("NameError", "Exception"),
    ("UnboundLocalError", "NameError"),
    ("AttributeError", "Exception"),
    ("AssertionError", "Exception"),
    ("StopIteration", "Exception"),
    ("OSError", "Exception"),
    ("TimeoutError", "OSError"),
];

/// What an `except` clause takes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Catches {
    /// Every raised value: a bare `except` and `except BaseException`.
    Everything,
    /// What `except Exception` takes: all but a cancellation and the exits.
    Exceptions,
    /// These classes, subclasses written out.
    Named(Vec<String>),
}

#[derive(Clone, Debug)]
pub(crate) struct Classes {
    parents: BTreeMap<String, String>,
}

impl Classes {
    pub(crate) fn builtin() -> Self {
        Self {
            parents: BUILTIN
                .iter()
                .map(|(name, parent)| ((*name).to_string(), (*parent).to_string()))
                .collect(),
        }
    }

    pub(crate) fn contains(&self, name: &str) -> bool {
        self.parents.contains_key(name)
    }

    /// Declares `class name(parent)`.
    pub(crate) fn declare(&mut self, name: &str, parent: &str) {
        self.parents.insert(name.to_string(), parent.to_string());
    }

    pub(crate) fn descends(&self, name: &str, ancestor: &str) -> bool {
        let mut current = name;
        loop {
            if current == ancestor {
                return true;
            }
            match self.parents.get(current) {
                Some(parent) if !parent.is_empty() => current = parent,
                _ => return false,
            }
        }
    }

    /// Whether `except Exception` takes the class.
    pub(crate) fn is_exception(&self, name: &str) -> bool {
        self.descends(name, "Exception")
    }

    /// What catching each of `names` takes.
    pub(crate) fn catches(&self, names: &[String]) -> Catches {
        if names.iter().any(|name| name == "BaseException") {
            return Catches::Everything;
        }
        let exceptions = names.iter().any(|name| name == "Exception");
        let mut taken: Vec<String> = self
            .parents
            .keys()
            .filter(|class| names.iter().any(|name| self.descends(class, name)))
            .filter(|class| !(exceptions && self.is_exception(class)))
            .cloned()
            .collect();
        taken.sort();
        if !exceptions {
            return Catches::Named(taken);
        }
        if taken.is_empty() {
            Catches::Exceptions
        } else {
            // `except (Exception, CancelledError)` and the like.
            Catches::Everything
        }
    }
}
