//! Patterns and flags: what is accepted, and the engine that compiles them.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

/// The most UTF-16 code units a pattern may have.
pub const MAX_PATTERN_UNITS: usize = 4_096;

/// The deepest a pattern's groups may nest.
pub const MAX_GROUP_NESTING: usize = 32;

/// The flags of a regex. `d` and `v` are refused.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Flags {
    pub global: bool,
    pub ignore_case: bool,
    pub multiline: bool,
    pub dot_all: bool,
    pub unicode: bool,
    pub sticky: bool,
}

impl Flags {
    /// Reads a flags text: each of `g`, `i`, `m`, `s`, `u`, `y` at most
    /// once, in any order.
    pub fn parse(text: &str) -> Result<Self, SyntaxError> {
        let mut flags = Self::default();
        for flag in text.chars() {
            let slot = match flag {
                'g' => &mut flags.global,
                'i' => &mut flags.ignore_case,
                'm' => &mut flags.multiline,
                's' => &mut flags.dot_all,
                'u' => &mut flags.unicode,
                'y' => &mut flags.sticky,
                'd' | 'v' => return Err(SyntaxError::UnsupportedFlag { flag }),
                _ => return Err(SyntaxError::UnknownFlag { flag }),
            };
            if *slot {
                return Err(SyntaxError::RepeatedFlag { flag });
            }
            *slot = true;
        }
        Ok(flags)
    }

    /// The flags in ECMAScript's order, `gimsuy`.
    pub fn canonical(self) -> String {
        [
            (self.global, 'g'),
            (self.ignore_case, 'i'),
            (self.multiline, 'm'),
            (self.dot_all, 's'),
            (self.unicode, 'u'),
            (self.sticky, 'y'),
        ]
        .into_iter()
        .filter_map(|(set, flag)| set.then_some(flag))
        .collect()
    }
}

/// Why a pattern or its flags are refused. Every function raises it as an
/// error of kind [`crate::SYNTAX_ERROR`], with this text as the message.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SyntaxError {
    #[error("invalid regular expression flags: `{flag}` is not a flag")]
    UnknownFlag { flag: char },
    #[error("invalid regular expression flags: `{flag}` is given twice")]
    RepeatedFlag { flag: char },
    #[error("invalid regular expression flags: `{flag}` is not supported")]
    UnsupportedFlag { flag: char },
    #[error(
        "invalid regular expression: the pattern has {units} UTF-16 code units, more than {MAX_PATTERN_UNITS}"
    )]
    PatternTooLong { units: usize },
    #[error("invalid regular expression: groups nest deeper than {MAX_GROUP_NESTING}")]
    PatternTooDeep,
    #[error("invalid regular expression: /{pattern}/: {detail}")]
    Pattern { pattern: String, detail: String },
}

/// Checks a pattern's length and nesting, the two bounds that keep
/// compiling it cheap. Decided from the text alone, before any compile.
fn check_shape(pattern: &str) -> Result<(), SyntaxError> {
    let units = pattern.encode_utf16().count();
    if units > MAX_PATTERN_UNITS {
        return Err(SyntaxError::PatternTooLong { units });
    }
    let mut depth = 0_usize;
    let mut escaped = false;
    let mut in_class = false;
    for character in pattern.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        match character {
            '\\' => escaped = true,
            '[' if !in_class => in_class = true,
            ']' if in_class => in_class = false,
            '(' if !in_class => {
                depth += 1;
                if depth > MAX_GROUP_NESTING {
                    return Err(SyntaxError::PatternTooDeep);
                }
            }
            ')' if !in_class => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    Ok(())
}

/// What decides a compiled program: the pattern and the flags the compiler
/// reads. `g` and `y` change how a program is driven, not the program.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Key {
    pattern: Arc<str>,
    ignore_case: bool,
    multiline: bool,
    dot_all: bool,
    unicode: bool,
}

/// A compiled program, or the compiler's refusal.
type Compiled = Result<Arc<lash_regress::Regex>, Arc<str>>;

fn compile(key: &Key) -> Compiled {
    lash_regress::Regex::with_flags(
        &key.pattern,
        lash_regress::Flags {
            icase: key.ignore_case,
            multiline: key.multiline,
            dot_all: key.dot_all,
            unicode: key.unicode,
            unicode_sets: false,
            no_opt: false,
        },
    )
    .map(Arc::new)
    .map_err(|error| error.text.into())
}

#[derive(Debug, Default)]
struct Cache {
    /// Counts lookups; an entry's stamp is the last lookup that touched it.
    clock: u64,
    entries: HashMap<Key, (Compiled, u64)>,
}

/// The regex engine an embedder owns: the compiler and its cache of
/// compiled patterns.
///
/// The cache holds at most the number of patterns the embedder states and
/// drops the least recently used. It is the embedder's memory, never the
/// run's, and nothing a function returns, charges or refuses depends on
/// what it holds: a compiled program is a function of its pattern and
/// flags, and a call compiles whatever the cache lacks.
#[derive(Debug)]
pub struct Engine {
    capacity: usize,
    cache: Mutex<Cache>,
}

impl Engine {
    /// An engine that keeps at most `cached_patterns` compiled patterns;
    /// zero keeps none.
    pub fn new(cached_patterns: usize) -> Self {
        Self {
            capacity: cached_patterns,
            cache: Mutex::new(Cache::default()),
        }
    }

    /// How many compiled patterns the cache holds now.
    pub fn cached_patterns(&self) -> usize {
        self.lock().entries.len()
    }

    /// Checks a pattern and its flags as every function does, without
    /// running anything: what a front end calls on a regex literal.
    pub fn check(&self, pattern: &str, flags: &str) -> Result<Flags, SyntaxError> {
        let flags = Flags::parse(flags)?;
        self.program(pattern, flags)?;
        Ok(flags)
    }

    pub(crate) fn program(
        &self,
        pattern: &str,
        flags: Flags,
    ) -> Result<Arc<lash_regress::Regex>, SyntaxError> {
        check_shape(pattern)?;
        let key = Key {
            pattern: pattern.into(),
            ignore_case: flags.ignore_case,
            multiline: flags.multiline,
            dot_all: flags.dot_all,
            unicode: flags.unicode,
        };
        self.compiled(key).map_err(|detail| SyntaxError::Pattern {
            pattern: pattern.to_string(),
            detail: detail.to_string(),
        })
    }

    fn compiled(&self, key: Key) -> Compiled {
        {
            let mut cache = self.lock();
            cache.clock += 1;
            let now = cache.clock;
            if let Some((compiled, stamp)) = cache.entries.get_mut(&key) {
                *stamp = now;
                return compiled.clone();
            }
        }
        // Compiled outside the lock: two callers may compile one pattern,
        // and the programs are the same.
        let compiled = compile(&key);
        if self.capacity == 0 {
            return compiled;
        }
        let mut cache = self.lock();
        while cache.entries.len() >= self.capacity && !cache.entries.contains_key(&key) {
            let Some(oldest) = cache
                .entries
                .iter()
                .min_by_key(|(_, (_, stamp))| *stamp)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            cache.entries.remove(&oldest);
        }
        let now = cache.clock;
        cache.entries.insert(key, (compiled.clone(), now));
        compiled
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Cache> {
        // The cache is consistent between statements, so a holder that
        // panicked left nothing half-written.
        self.cache.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// `units` as text, unless a surrogate stands alone in them.
pub(crate) fn text_from_units(units: &[u16]) -> Option<String> {
    String::from_utf16(units).ok()
}
