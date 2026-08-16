use super::javascript::{javascript_array_method, js_stdlib_error, utf16_value};
use super::*;
use crate::runtime::{
    ErrorKind, canonical_regexp_flags, ensure_javascript_string_size, javascript_array_index_key,
    javascript_string_size_error,
};

pub const TYPESCRIPT_REGEXP_MAX_PATTERN_CODE_UNITS: usize = 4_096;
pub const TYPESCRIPT_REGEXP_MAX_NESTING: usize = 32;
pub const TYPESCRIPT_REGEXP_EXECUTION_FUEL: u64 = 1_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TypeScriptRegExpValidationError {
    PatternTooLong,
    PatternTooDeep,
    InvalidFlags,
    UnsupportedFlag(char),
    InvalidPattern,
}

impl TypeScriptRegExpValidationError {
    pub const fn diagnostic_code(self) -> &'static str {
        match self {
            Self::PatternTooLong => "TS_REGEX_PATTERN_TOO_LONG",
            Self::PatternTooDeep => "TS_REGEX_PATTERN_NESTING_LIMIT",
            Self::InvalidFlags | Self::InvalidPattern => "TS_REGEX_INVALID",
            Self::UnsupportedFlag('d') => "TS_REGEX_INDICES_FLAG_UNSUPPORTED",
            Self::UnsupportedFlag('v') => "TS_REGEX_UNICODE_SETS_FLAG_UNSUPPORTED",
            Self::UnsupportedFlag(_) => "TS_REGEX_FLAG_UNSUPPORTED",
        }
    }
}

pub fn validate_typescript_regexp(
    pattern: &str,
    flags: &str,
) -> Result<(), TypeScriptRegExpValidationError> {
    validate_regexp_flags(flags)?;
    validate_typescript_regexp_shape(pattern)?;
    compile_regexp(pattern, flags)
        .map(|_| ())
        .map_err(|_| TypeScriptRegExpValidationError::InvalidPattern)
}

fn validate_regexp_flags(flags: &str) -> Result<(), TypeScriptRegExpValidationError> {
    let mut seen = [false; 6];
    for flag in flags.chars() {
        let index = match flag {
            'g' => 0,
            'i' => 1,
            'm' => 2,
            's' => 3,
            'u' => 4,
            'y' => 5,
            'd' | 'v' => return Err(TypeScriptRegExpValidationError::UnsupportedFlag(flag)),
            _ => return Err(TypeScriptRegExpValidationError::InvalidFlags),
        };
        if seen[index] {
            return Err(TypeScriptRegExpValidationError::InvalidFlags);
        }
        seen[index] = true;
    }
    Ok(())
}

pub fn validate_typescript_regexp_shape(
    pattern: &str,
) -> Result<(), TypeScriptRegExpValidationError> {
    if pattern.encode_utf16().count() > TYPESCRIPT_REGEXP_MAX_PATTERN_CODE_UNITS {
        return Err(TypeScriptRegExpValidationError::PatternTooLong);
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
                if depth > TYPESCRIPT_REGEXP_MAX_NESTING {
                    return Err(TypeScriptRegExpValidationError::PatternTooDeep);
                }
            }
            ')' if !in_class => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    Ok(())
}

fn compile_regexp(pattern: &str, flags: &str) -> Result<regress::Regex, regress::Error> {
    regress::Regex::with_flags(
        pattern,
        regress::Flags {
            icase: flags.contains('i'),
            multiline: flags.contains('m'),
            dot_all: flags.contains('s'),
            unicode: flags.contains('u'),
            unicode_sets: false,
            no_opt: false,
        },
    )
}

#[derive(Clone)]
struct CapturedMatch {
    range: std::ops::Range<usize>,
    captures: Vec<Option<std::ops::Range<usize>>>,
    named: Vec<(String, Option<std::ops::Range<usize>>)>,
}

impl From<regress::Match> for CapturedMatch {
    fn from(found: regress::Match) -> Self {
        let named = found
            .named_groups()
            .map(|(name, range)| (name.to_string(), range))
            .collect();
        Self {
            range: found.range,
            captures: found.captures,
            named,
        }
    }
}

fn collect_regress_match(
    found: Result<regress::Match, regress::MatchError>,
) -> Result<CapturedMatch, RuntimeError> {
    found
        .map(CapturedMatch::from)
        .map_err(
            |regress::MatchError::Exhausted| RuntimeError::RegExpBudgetExceeded {
                limit: TYPESCRIPT_REGEXP_EXECUTION_FUEL,
            },
        )
}

fn collect_bounded_regress_matches<I>(
    heap: &Heap,
    matches: I,
    input: &[u16],
    unicode: bool,
    sticky: bool,
    start: usize,
    max_matches: Option<usize>,
) -> Result<Vec<CapturedMatch>, RuntimeError>
where
    I: Iterator<Item = Result<regress::Match, regress::MatchError>>,
{
    let mut collected = Vec::new();
    let mut transient_bytes = 0_u64;
    let mut expected = start;
    let mut matches = matches;
    loop {
        if max_matches.is_some_and(|limit| collected.len() >= limit) {
            break;
        }
        let Some(found) = matches.next() else {
            break;
        };
        let found = collect_regress_match(found)?;
        if sticky && found.range.start != expected {
            break;
        }
        transient_bytes = transient_bytes
            .saturating_add(std::mem::size_of::<CapturedMatch>() as u64)
            .saturating_add(
                (found.captures.capacity() as u64)
                    .saturating_mul(std::mem::size_of::<Option<std::ops::Range<usize>>>() as u64),
            )
            .saturating_add(found.named.iter().fold(0_u64, |bytes, (name, _)| {
                bytes
                    .saturating_add(
                        std::mem::size_of::<(String, Option<std::ops::Range<usize>>)>() as u64,
                    )
                    .saturating_add(name.capacity() as u64)
            }));
        heap.ensure_additional_logical_bytes(transient_bytes)?;
        collected
            .try_reserve_exact(1)
            .map_err(|_| RuntimeError::MemoryLimitExceeded {
                limit: DEFAULT_HEAP_LOGICAL_BYTE_LIMIT,
                attempted: u64::MAX,
            })?;
        expected = if found.range.is_empty() {
            advance_string_index(input, found.range.end, unicode)
        } else {
            found.range.end
        };
        collected.push(found);
    }
    Ok(collected)
}

impl<H: ExecutionHost> Vm<'_, H> {
    pub(super) fn try_execute_regexp_match_stdlib(
        &mut self,
        values: &[Value],
    ) -> Result<bool, RuntimeError> {
        if let [Value::String(method), Value::Ref(receiver)] = values
            && let HeapObject::RegExpMatch(result) = self.heap.get(*receiver)?
        {
            let value = match method.as_str() {
                "Object.keys" => Some(Value::List(
                    result
                        .enumerable_keys()
                        .into_iter()
                        .map(|key| Value::String(key.into()))
                        .collect::<Vec<_>>()
                        .into(),
                )),
                "Object.values" => Some(Value::List(result.enumerable_values().into())),
                "Object.entries" => Some(Value::List(
                    result
                        .enumerable_entries()
                        .into_iter()
                        .map(|(key, value)| {
                            Value::List(vec![Value::String(key.into()), value].into())
                        })
                        .collect::<Vec<_>>()
                        .into(),
                )),
                "Array.isArray" => Some(Value::Bool(true)),
                "Lash.ArrayFromIterable" => Some(Value::List(result.items.clone().into())),
                _ => None,
            };
            if let Some(value) = value {
                self.stack.push(value);
                return Ok(true);
            }
        }
        if let [Value::String(method), Value::Ref(receiver), key] = values
            && method.as_str() == "Object.hasOwn"
            && let HeapObject::RegExpMatch(result) = self.heap.get(*receiver)?
        {
            let key = self.heap.javascript_to_string(key)?;
            self.stack.push(Value::Bool(
                matches!(key.as_str(), "length" | "index" | "input" | "groups")
                    || javascript_array_index_key(&key)
                        .is_some_and(|index| index < result.items.len()),
            ));
            return Ok(true);
        }
        Ok(false)
    }

    pub(super) fn try_execute_regexp_match_method(
        &mut self,
        method: &str,
        receiver: HeapId,
        args: &[Value],
    ) -> Result<bool, RuntimeError> {
        let HeapObject::RegExpMatch(result) = self.heap.get(receiver)? else {
            return Ok(false);
        };
        let value = javascript_array_method(method, &result.items, args)?;
        self.stack.push(value);
        Ok(true)
    }

    pub(super) fn construct_regexp(&mut self, args: &[Value]) -> Result<Value, RuntimeError> {
        let (pattern, flags) = match args {
            [] | [Value::Undefined] => (String::new(), String::new()),
            [Value::String(pattern)] => (pattern.to_string(), String::new()),
            [Value::String(pattern), Value::Undefined] => (pattern.to_string(), String::new()),
            [Value::Undefined, Value::String(flags)] => (String::new(), flags.to_string()),
            [Value::String(pattern), Value::String(flags)] => {
                (pattern.to_string(), flags.to_string())
            }
            [_, ..] if args.len() <= 2 => {
                return Err(self.regexp_type_error(
                    "TS_REGEX_CONSTRUCTOR_STRING_REQUIRED: RegExp pattern and flags must be strings or undefined; pass an explicit string",
                ));
            }
            _ => {
                return Err(js_stdlib_error(format!(
                    "RegExp constructor received {} arguments",
                    args.len()
                )));
            }
        };
        if let Err(error) =
            validate_regexp_flags(&flags).and_then(|()| validate_typescript_regexp_shape(&pattern))
        {
            return Err(self.regexp_syntax_error(error, &pattern, &flags, None));
        }
        let program = compile_regexp(&pattern, &flags).map_err(|error| {
            self.regexp_syntax_error(
                TypeScriptRegExpValidationError::InvalidPattern,
                &pattern,
                &flags,
                Some(&error.to_string()),
            )
        })?;
        let flags = canonical_regexp_flags(&flags).map_err(js_stdlib_error)?;
        let value = self.heap.allocate_regexp(pattern, flags)?;
        let Value::Ref(receiver) = value else {
            unreachable!("RegExp allocation produces a heap reference")
        };
        self.heap.set_regexp_program(receiver, program)?;
        Ok(Value::Ref(receiver))
    }

    fn regexp_syntax_error(
        &mut self,
        error: TypeScriptRegExpValidationError,
        pattern: &str,
        flags: &str,
        detail: Option<&str>,
    ) -> RuntimeError {
        let message = match error {
            TypeScriptRegExpValidationError::PatternTooLong => format!(
                "TS_REGEX_PATTERN_TOO_LONG: Invalid regular expression: /{pattern}/: pattern exceeds {TYPESCRIPT_REGEXP_MAX_PATTERN_CODE_UNITS} UTF-16 code units; split the pattern into smaller expressions"
            ),
            TypeScriptRegExpValidationError::PatternTooDeep => format!(
                "TS_REGEX_PATTERN_NESTING_LIMIT: Invalid regular expression: /{pattern}/: pattern nesting exceeds {TYPESCRIPT_REGEXP_MAX_NESTING}; split the pattern into smaller expressions"
            ),
            TypeScriptRegExpValidationError::UnsupportedFlag(flag) => {
                let repair = if flag == 'd' {
                    "; remove `d` and use match.index plus capture lengths"
                } else {
                    "; replace `v` with `u` and ordinary Unicode character classes"
                };
                format!(
                    "{}: Invalid flags supplied to RegExp constructor '{flags}'",
                    TypeScriptRegExpValidationError::UnsupportedFlag(flag).diagnostic_code()
                ) + repair
            }
            TypeScriptRegExpValidationError::InvalidFlags => {
                format!("Invalid flags supplied to RegExp constructor '{flags}'")
            }
            TypeScriptRegExpValidationError::InvalidPattern => detail.map_or_else(
                || format!("Invalid regular expression: /{pattern}/"),
                |detail| {
                    format!(
                        "Invalid regular expression: /{pattern}/: {}",
                        node_regexp_error_detail(pattern, detail)
                    )
                },
            ),
        };
        match self
            .heap
            .allocate_error(ErrorKind::SyntaxError, message, None, None)
        {
            Ok(value) => RuntimeError::UncaughtException { value },
            Err(error) => error,
        }
    }

    fn regexp_program(&mut self, receiver: HeapId) -> Result<regress::Regex, RuntimeError> {
        let (pattern, flags, cached) = match self.heap.get(receiver)? {
            HeapObject::RegExp(regexp) => (
                regexp.pattern.clone(),
                regexp.flags.clone(),
                regexp
                    .compiled_program
                    .as_ref()
                    .map(|cache| cache.program.clone()),
            ),
            object => {
                return Err(js_stdlib_error(format!(
                    "RegExp method requires a RegExp receiver, got {}",
                    object.kind_name()
                )));
            }
        };
        if let Some(program) = cached {
            return Ok(program);
        }
        let program = compile_regexp(&pattern, &flags).map_err(|_| {
            self.regexp_syntax_error(
                TypeScriptRegExpValidationError::InvalidPattern,
                &pattern,
                &flags,
                None,
            )
        })?;
        self.heap.set_regexp_program(receiver, program.clone())?;
        Ok(program)
    }

    fn regexp_matches(
        &mut self,
        receiver: HeapId,
        input: &[u16],
        start: usize,
        honor_sticky: bool,
        max_matches: Option<usize>,
    ) -> Result<Vec<CapturedMatch>, RuntimeError> {
        let program = self.regexp_program(receiver)?;
        let unicode = match self.heap.get(receiver)? {
            HeapObject::RegExp(regexp) => regexp.flags.contains('u'),
            _ => unreachable!(),
        };
        let sticky = honor_sticky
            && matches!(self.heap.get(receiver)?, HeapObject::RegExp(re) if re.flags.contains('y'));
        if unicode {
            let matches = if sticky {
                program.try_find_from_utf16_anchored(input, start, TYPESCRIPT_REGEXP_EXECUTION_FUEL)
            } else {
                program.try_find_from_utf16(input, start, TYPESCRIPT_REGEXP_EXECUTION_FUEL)
            };
            collect_bounded_regress_matches(
                &self.heap,
                matches,
                input,
                unicode,
                sticky,
                start,
                max_matches,
            )
        } else {
            let matches = if sticky {
                program.try_find_from_ucs2_anchored(input, start, TYPESCRIPT_REGEXP_EXECUTION_FUEL)
            } else {
                program.try_find_from_ucs2(input, start, TYPESCRIPT_REGEXP_EXECUTION_FUEL)
            };
            collect_bounded_regress_matches(
                &self.heap,
                matches,
                input,
                unicode,
                sticky,
                start,
                max_matches,
            )
        }
    }

    fn first_regexp_match(
        &mut self,
        receiver: HeapId,
        input: &[u16],
        start: usize,
        sticky: bool,
    ) -> Result<Option<CapturedMatch>, RuntimeError> {
        let program = self.regexp_program(receiver)?;
        let unicode = matches!(self.heap.get(receiver)?, HeapObject::RegExp(regexp) if regexp.flags.contains('u'));
        let found = if unicode && sticky {
            program
                .try_find_from_utf16_anchored(input, start, TYPESCRIPT_REGEXP_EXECUTION_FUEL)
                .next()
                .transpose()
        } else if unicode {
            program
                .try_find_from_utf16(input, start, TYPESCRIPT_REGEXP_EXECUTION_FUEL)
                .next()
                .transpose()
        } else if sticky {
            program
                .try_find_from_ucs2_anchored(input, start, TYPESCRIPT_REGEXP_EXECUTION_FUEL)
                .next()
                .transpose()
        } else {
            program
                .try_find_from_ucs2(input, start, TYPESCRIPT_REGEXP_EXECUTION_FUEL)
                .next()
                .transpose()
        }
        .map_err(
            |regress::MatchError::Exhausted| RuntimeError::RegExpBudgetExceeded {
                limit: TYPESCRIPT_REGEXP_EXECUTION_FUEL,
            },
        )?
        .map(CapturedMatch::from);
        Ok(found)
    }

    fn allocate_match_result(
        &mut self,
        input: &str,
        units: &[u16],
        found: &CapturedMatch,
    ) -> Result<Value, RuntimeError> {
        let expected_bytes = regexp_match_allocation_bytes(input, units, found)?;
        self.heap.ensure_additional_logical_bytes(expected_bytes)?;

        let mut items = Vec::new();
        items
            .try_reserve_exact(found.captures.len() + 1)
            .map_err(|_| RuntimeError::MemoryLimitExceeded {
                limit: expected_bytes,
                attempted: expected_bytes,
            })?;
        items.push(utf16_range(units, found.range.clone())?);
        for range in &found.captures {
            items.push(
                range
                    .clone()
                    .map(|range| utf16_range(units, range))
                    .transpose()?
                    .unwrap_or(Value::Undefined),
            );
        }
        let groups = if found.named.is_empty() {
            Value::Undefined
        } else {
            let Value::Record(groups) = self.named_groups_value(units, &found.named)? else {
                unreachable!("named groups produce a record")
            };
            self.heap.allocate_record((*groups).clone())?
        };
        self.heap.allocate_regexp_match(
            items,
            Value::Number(found.range.start as f64),
            Value::String(input.into()),
            groups,
        )
    }

    fn named_groups_value(
        &mut self,
        units: &[u16],
        named: &[(String, Option<std::ops::Range<usize>>)],
    ) -> Result<Value, RuntimeError> {
        if named.is_empty() {
            return Ok(Value::Undefined);
        }
        let mut groups = record_with_capacity(named.len());
        for (name, range) in named {
            groups.insert(
                name.clone(),
                range
                    .clone()
                    .map(|range| utf16_range(units, range))
                    .transpose()?
                    .unwrap_or(Value::Undefined),
            );
        }
        Ok(Value::Record(Arc::new(groups)))
    }

    fn exec_regexp(&mut self, receiver: HeapId, input: &str) -> Result<Value, RuntimeError> {
        let units = bounded_utf16_input(&self.heap, input)?;
        let (global, sticky, start) = match self.heap.get(receiver)? {
            HeapObject::RegExp(regexp) => (
                regexp.flags.contains('g'),
                regexp.flags.contains('y'),
                regexp.last_index as usize,
            ),
            _ => return Err(js_stdlib_error("RegExp.exec requires a RegExp receiver")),
        };
        let stateful = global || sticky;
        let start = if stateful { start } else { 0 };
        if start > units.len() {
            if stateful {
                self.heap.set_regexp_last_index(receiver, 0)?;
            }
            return Ok(Value::Null);
        }
        let found = self.first_regexp_match(receiver, &units, start, sticky)?;
        let Some(found) = found else {
            if stateful {
                self.heap.set_regexp_last_index(receiver, 0)?;
            }
            return Ok(Value::Null);
        };
        if stateful {
            self.heap
                .set_regexp_last_index(receiver, found.range.end as u64)?;
        }
        self.allocate_match_result(input, &units, &found)
    }

    pub(super) fn execute_javascript_regexp(&mut self, argc: usize) -> Result<(), RuntimeError> {
        self.require_typescript_intrinsic("JavaScript RegExp")?;
        let values = self.pop_n(argc)?;
        let Some(Value::String(operation)) = values.first() else {
            return Err(js_stdlib_error("missing RegExp operation discriminator"));
        };
        let args = &values[1..];
        let result = match (operation.as_str(), args) {
            ("exec", [Value::Ref(receiver), input]) => {
                let input = self.heap.javascript_to_string(input)?;
                self.exec_regexp(*receiver, &input)?
            }
            ("test", [Value::Ref(receiver), input]) => {
                let input = self.heap.javascript_to_string(input)?;
                Value::Bool(!matches!(self.exec_regexp(*receiver, &input)?, Value::Null))
            }
            ("match", [input, Value::Ref(receiver)]) => {
                let input = self.heap.javascript_to_string(input)?;
                self.string_match(&input, *receiver)?
            }
            ("search", [input, Value::Ref(receiver)]) => {
                let input = self.heap.javascript_to_string(input)?;
                Value::Number(self.string_search(&input, *receiver)? as f64)
            }
            ("matchAll", [input, Value::Ref(receiver)]) => {
                let input = self.heap.javascript_to_string(input)?;
                self.string_match_all(&input, *receiver)?
            }
            ("split", [input, separator, limit]) => {
                let input = self.heap.javascript_to_string(input)?;
                self.string_split(&input, separator, limit)?
            }
            ("replaceString", [input, search, replacement, all]) => {
                let input = self.heap.javascript_to_string(input)?;
                let replacement = self.heap.javascript_to_string(replacement)?;
                let Value::Bool(all) = all else {
                    return Err(js_stdlib_error("replace all discriminator must be boolean"));
                };
                self.string_replace(&input, search, &replacement, *all)?
            }
            ("replacePlan", [input, search, all]) => {
                let input = self.heap.javascript_to_string(input)?;
                let Value::Bool(all) = all else {
                    return Err(js_stdlib_error("replace all discriminator must be boolean"));
                };
                self.replacement_plan(&input, search, *all)?
            }
            ("replaceFinish", [input, plan, results]) => {
                let input = self.heap.javascript_to_string(input)?;
                self.finish_replacement(&input, plan, results)?
            }
            _ => {
                return Err(js_stdlib_error(format!(
                    "TS_REGEX_OPERATION_UNSUPPORTED: {operation} with {} argument(s)",
                    args.len()
                )));
            }
        };
        self.stack.push(result);
        Ok(())
    }

    fn string_match(&mut self, input: &str, receiver: HeapId) -> Result<Value, RuntimeError> {
        let global =
            matches!(self.heap.get(receiver)?, HeapObject::RegExp(re) if re.flags.contains('g'));
        if !global {
            return self.exec_regexp(receiver, input);
        }
        self.heap.set_regexp_last_index(receiver, 0)?;
        let units = bounded_utf16_input(&self.heap, input)?;
        let matches = self.regexp_matches(receiver, &units, 0, true, None)?;
        let mut values = Vec::new();
        let mut pending_bytes = 16_u64;
        for found in &matches {
            push_utf16_range_bounded(
                &self.heap,
                &mut values,
                &units,
                found.range.clone(),
                &mut pending_bytes,
            )?;
        }
        self.heap.set_regexp_last_index(receiver, 0)?;
        if values.is_empty() {
            Ok(Value::Null)
        } else {
            Ok(Value::List(values.into()))
        }
    }

    fn string_search(&mut self, input: &str, receiver: HeapId) -> Result<i64, RuntimeError> {
        let units = bounded_utf16_input(&self.heap, input)?;
        let saved = self.heap.regexp_last_index(receiver)?.unwrap_or(0);
        let sticky =
            matches!(self.heap.get(receiver)?, HeapObject::RegExp(re) if re.flags.contains('y'));
        let found = self.first_regexp_match(receiver, &units, 0, sticky)?;
        self.heap.set_regexp_last_index(receiver, saved)?;
        Ok(found.map_or(-1, |found| found.range.start as i64))
    }

    fn string_match_all(&mut self, input: &str, receiver: HeapId) -> Result<Value, RuntimeError> {
        let (global, unicode, sticky, start) = match self.heap.get(receiver)? {
            HeapObject::RegExp(regexp) => (
                regexp.flags.contains('g'),
                regexp.flags.contains('u'),
                regexp.flags.contains('y'),
                regexp.last_index as usize,
            ),
            _ => return Err(js_stdlib_error("matchAll requires a RegExp")),
        };
        if !global {
            return Err(self.regexp_type_error(
                "String.prototype.matchAll called with a non-global RegExp argument",
            ));
        }
        let units = bounded_utf16_input(&self.heap, input)?;
        let program = self.regexp_program(receiver)?;
        if unicode && sticky {
            self.collect_match_all_values(
                input,
                &units,
                program.try_find_from_utf16_anchored(
                    &units,
                    start,
                    TYPESCRIPT_REGEXP_EXECUTION_FUEL,
                ),
                unicode,
                sticky,
                start,
            )
        } else if unicode {
            self.collect_match_all_values(
                input,
                &units,
                program.try_find_from_utf16(&units, start, TYPESCRIPT_REGEXP_EXECUTION_FUEL),
                unicode,
                sticky,
                start,
            )
        } else if sticky {
            self.collect_match_all_values(
                input,
                &units,
                program.try_find_from_ucs2_anchored(
                    &units,
                    start,
                    TYPESCRIPT_REGEXP_EXECUTION_FUEL,
                ),
                unicode,
                sticky,
                start,
            )
        } else {
            self.collect_match_all_values(
                input,
                &units,
                program.try_find_from_ucs2(&units, start, TYPESCRIPT_REGEXP_EXECUTION_FUEL),
                unicode,
                sticky,
                start,
            )
        }
    }

    fn collect_match_all_values<I>(
        &mut self,
        input: &str,
        units: &[u16],
        matches: I,
        unicode: bool,
        sticky: bool,
        start: usize,
    ) -> Result<Value, RuntimeError>
    where
        I: Iterator<Item = Result<regress::Match, regress::MatchError>>,
    {
        let mut values = Vec::new();
        let mut expected = start;
        for found in matches {
            let found = collect_regress_match(found)?;
            if sticky && found.range.start != expected {
                break;
            }
            self.heap.ensure_additional_logical_bytes(
                16_u64.saturating_add((values.len() as u64 + 1).saturating_mul(24)),
            )?;
            values
                .try_reserve_exact(1)
                .map_err(|_| RuntimeError::MemoryLimitExceeded {
                    limit: DEFAULT_HEAP_LOGICAL_BYTE_LIMIT,
                    attempted: u64::MAX,
                })?;
            expected = if found.range.is_empty() {
                advance_string_index(units, found.range.end, unicode)
            } else {
                found.range.end
            };
            values.push(self.allocate_match_result(input, units, &found)?);
        }
        Ok(Value::List(values.into()))
    }

    fn string_split(
        &mut self,
        input: &str,
        separator: &Value,
        limit: &Value,
    ) -> Result<Value, RuntimeError> {
        let limit = if matches!(limit, Value::Undefined) {
            u32::MAX
        } else {
            to_uint32(self.heap.javascript_to_number(limit)?)
        };
        if limit == 0 {
            return Ok(Value::List(Vec::new().into()));
        }
        let Value::Ref(receiver) = separator else {
            return super::javascript::javascript_string_method(
                "split",
                input,
                &[separator.clone(), Value::Number(limit as f64)],
            );
        };
        if !matches!(self.heap.get(*receiver)?, HeapObject::RegExp(_)) {
            return Err(js_stdlib_error("split separator is not a RegExp"));
        }
        let units = bounded_utf16_input(&self.heap, input)?;
        if limit == 1 && !units.is_empty() {
            let unicode = matches!(
                self.heap.get(*receiver)?,
                HeapObject::RegExp(regexp) if regexp.flags.contains('u')
            );
            let mut found = self.first_regexp_match(*receiver, &units, 0, false)?;
            if found.as_ref().is_some_and(|found| found.range.is_empty()) {
                found = self.first_regexp_match(
                    *receiver,
                    &units,
                    advance_string_index(&units, 0, unicode),
                    false,
                )?;
            }
            let end = found
                .filter(|found| !(found.range.start == units.len() && found.range.is_empty()))
                .map_or(units.len(), |found| found.range.start);
            let mut output = Vec::new();
            let mut pending_bytes = 16_u64;
            push_utf16_range_bounded(&self.heap, &mut output, &units, 0..end, &mut pending_bytes)?;
            return Ok(Value::List(output.into()));
        }
        let program = self.regexp_program(*receiver)?;
        let unicode = matches!(
            self.heap.get(*receiver)?,
            HeapObject::RegExp(regexp) if regexp.flags.contains('u')
        );
        if unicode {
            self.collect_split_values(
                &units,
                limit as usize,
                program.try_find_from_utf16(&units, 0, TYPESCRIPT_REGEXP_EXECUTION_FUEL),
            )
        } else {
            self.collect_split_values(
                &units,
                limit as usize,
                program.try_find_from_ucs2(&units, 0, TYPESCRIPT_REGEXP_EXECUTION_FUEL),
            )
        }
    }

    fn collect_split_values<I>(
        &self,
        units: &[u16],
        limit: usize,
        mut matches: I,
    ) -> Result<Value, RuntimeError>
    where
        I: Iterator<Item = Result<regress::Match, regress::MatchError>>,
    {
        if units.is_empty() {
            let found = matches
                .next()
                .transpose()
                .map_err(
                    |regress::MatchError::Exhausted| RuntimeError::RegExpBudgetExceeded {
                        limit: TYPESCRIPT_REGEXP_EXECUTION_FUEL,
                    },
                )?;
            return Ok(Value::List(
                if found.is_some_and(|found| found.range().is_empty()) {
                    Vec::new()
                } else {
                    vec![Value::String(String::new().into())]
                }
                .into(),
            ));
        }
        let mut output = Vec::new();
        let mut pending_bytes = 16_u64;
        let mut end = 0;
        for found in &mut matches {
            let found = collect_regress_match(found)?;
            if found.range.start == units.len() && found.range.is_empty() {
                break;
            }
            if found.range.start < end
                || (found.range.start == found.range.end && found.range.start == end)
            {
                continue;
            }
            push_utf16_range_bounded(
                &self.heap,
                &mut output,
                units,
                end..found.range.start,
                &mut pending_bytes,
            )?;
            if output.len() >= limit {
                break;
            }
            for capture in found.captures {
                match capture {
                    Some(range) => push_utf16_range_bounded(
                        &self.heap,
                        &mut output,
                        units,
                        range,
                        &mut pending_bytes,
                    )?,
                    None => push_transient_value_bounded(
                        &self.heap,
                        &mut output,
                        Value::Undefined,
                        17,
                        &mut pending_bytes,
                    )?,
                }
                if output.len() >= limit {
                    break;
                }
            }
            end = found.range.end;
            if output.len() >= limit {
                break;
            }
        }
        if output.len() < limit {
            push_utf16_range_bounded(
                &self.heap,
                &mut output,
                units,
                end..units.len(),
                &mut pending_bytes,
            )?;
        }
        output.truncate(limit);
        Ok(Value::List(output.into()))
    }

    fn string_replace(
        &mut self,
        input: &str,
        search: &Value,
        replacement: &str,
        all: bool,
    ) -> Result<Value, RuntimeError> {
        let regexp = matches!(search, Value::Ref(receiver) if matches!(self.heap.get(*receiver)?, HeapObject::RegExp(_)));
        let lone_surrogate_error = lone_surrogate_output_error(regexp);
        let units = bounded_utf16_input(&self.heap, input)?;
        let matches = self.replacement_matches(input, search, all)?;
        let mut output = Vec::new();
        let mut output_bytes = 0;
        let mut end = 0;
        for found in &matches {
            append_utf16_checked(
                &self.heap,
                &mut output,
                &mut output_bytes,
                &units[end..found.range.start],
                lone_surrogate_error,
            )?;
            expand_replacement_checked(
                &self.heap,
                &mut output,
                &mut output_bytes,
                &units,
                replacement,
                found,
                lone_surrogate_error,
            )?;
            end = found.range.end;
        }
        append_utf16_checked(
            &self.heap,
            &mut output,
            &mut output_bytes,
            &units[end..],
            lone_surrogate_error,
        )?;
        utf16_value(output)
    }

    fn replacement_matches(
        &mut self,
        input: &str,
        search: &Value,
        all: bool,
    ) -> Result<Vec<CapturedMatch>, RuntimeError> {
        let units = bounded_utf16_input(&self.heap, input)?;
        match search {
            Value::Ref(receiver) if matches!(self.heap.get(*receiver)?, HeapObject::RegExp(_)) => {
                let global = matches!(self.heap.get(*receiver)?, HeapObject::RegExp(re) if re.flags.contains('g'));
                if all && !global {
                    return Err(self.regexp_type_error(
                        "String.prototype.replaceAll called with a non-global RegExp argument",
                    ));
                }
                if global {
                    self.heap.set_regexp_last_index(*receiver, 0)?;
                }
                let sticky = matches!(self.heap.get(*receiver)?, HeapObject::RegExp(re) if re.flags.contains('y'));
                let mut matches = if global {
                    self.regexp_matches(*receiver, &units, 0, true, None)?
                } else {
                    let start = if sticky {
                        self.heap.regexp_last_index(*receiver)?.unwrap_or(0)
                    } else {
                        0
                    };
                    let found =
                        self.first_regexp_match(*receiver, &units, start as usize, sticky)?;
                    if sticky {
                        self.heap.set_regexp_last_index(
                            *receiver,
                            found.as_ref().map_or(0, |found| found.range.end as u64),
                        )?;
                    }
                    found.into_iter().collect()
                };
                if !all && !global {
                    matches.truncate(1);
                }
                if global {
                    self.heap.set_regexp_last_index(*receiver, 0)?;
                }
                Ok(matches)
            }
            value => {
                let needle = self.heap.javascript_to_string(value)?;
                let needle = bounded_utf16_input(&self.heap, &needle)?;
                find_string_matches(&self.heap, &units, &needle, all)
            }
        }
    }

    fn replacement_plan(
        &mut self,
        input: &str,
        search: &Value,
        all: bool,
    ) -> Result<Value, RuntimeError> {
        let units = bounded_utf16_input(&self.heap, input)?;
        let regexp = matches!(search, Value::Ref(receiver) if matches!(self.heap.get(*receiver)?, HeapObject::RegExp(_)));
        let matches = self.replacement_matches(input, search, all)?;
        let mut plan = Vec::new();
        let mut pending_bytes = 16_u64;
        for found in matches {
            pending_bytes =
                pending_bytes.saturating_add(regexp_match_allocation_bytes(input, &units, &found)?);
            self.heap.ensure_additional_logical_bytes(pending_bytes)?;
            plan.try_reserve_exact(1)
                .map_err(|_| RuntimeError::MemoryLimitExceeded {
                    limit: DEFAULT_HEAP_LOGICAL_BYTE_LIMIT,
                    attempted: u64::MAX,
                })?;
            let mut arguments = vec![utf16_range(&units, found.range.clone())?];
            arguments.extend(
                found
                    .captures
                    .iter()
                    .map(|range| {
                        range
                            .clone()
                            .map(|range| utf16_range(&units, range))
                            .transpose()
                            .map(|value| value.unwrap_or(Value::Undefined))
                    })
                    .collect::<Result<Vec<_>, RuntimeError>>()?,
            );
            arguments.push(Value::Number(found.range.start as f64));
            arguments.push(Value::String(input.into()));
            if !found.named.is_empty() {
                arguments.push(self.named_groups_value(&units, &found.named)?);
            }
            plan.push(Value::List(
                vec![
                    Value::List(arguments.into()),
                    Value::Number(found.range.start as f64),
                    Value::Number(found.range.end as f64),
                    Value::Bool(regexp),
                ]
                .into(),
            ));
        }
        Ok(Value::List(plan.into()))
    }

    fn finish_replacement(
        &mut self,
        input: &str,
        plan: &Value,
        results: &Value,
    ) -> Result<Value, RuntimeError> {
        let plan = regexp_sequence(&self.heap, plan)
            .ok_or_else(|| js_stdlib_error("invalid RegExp replacement plan"))?;
        let results = regexp_sequence(&self.heap, results)
            .ok_or_else(|| js_stdlib_error("invalid RegExp replacement results"))?;
        if plan.len() != results.len() {
            return Err(js_stdlib_error("RegExp replacement result count mismatch"));
        }
        let units = bounded_utf16_input(&self.heap, input)?;
        let mut output = Vec::new();
        let mut output_bytes = 0;
        let mut end = 0_usize;
        let mut last_regexp = false;
        for (entry, result) in plan.iter().zip(results.iter()) {
            let entry = regexp_sequence(&self.heap, entry)
                .ok_or_else(|| js_stdlib_error("invalid RegExp replacement plan entry"))?;
            let [
                _,
                Value::Number(start),
                Value::Number(next_end),
                Value::Bool(regexp),
            ] = entry.as_slice()
            else {
                return Err(js_stdlib_error("invalid RegExp replacement plan range"));
            };
            let start = *start as usize;
            let next_end = *next_end as usize;
            last_regexp = *regexp;
            let lone_surrogate_error = lone_surrogate_output_error(*regexp);
            append_utf16_checked(
                &self.heap,
                &mut output,
                &mut output_bytes,
                &units[end..start],
                lone_surrogate_error,
            )?;
            let replacement = self.heap.javascript_to_string(result)?;
            let replacement_units = bounded_utf16_input(&self.heap, &replacement)?;
            append_utf16_checked(
                &self.heap,
                &mut output,
                &mut output_bytes,
                &replacement_units,
                lone_surrogate_error,
            )?;
            end = next_end;
        }
        append_utf16_checked(
            &self.heap,
            &mut output,
            &mut output_bytes,
            &units[end..],
            lone_surrogate_output_error(last_regexp),
        )?;
        utf16_value(output)
    }

    fn regexp_type_error(&mut self, message: &str) -> RuntimeError {
        match self
            .heap
            .allocate_error(ErrorKind::TypeError, message.to_string(), None, None)
        {
            Ok(value) => RuntimeError::UncaughtException { value },
            Err(error) => error,
        }
    }
}

fn regexp_sequence(heap: &Heap, value: &Value) -> Option<Vec<Value>> {
    match value {
        Value::List(values) | Value::Tuple(values) => Some(values.to_vec()),
        Value::Ref(id) => match heap.get(*id).ok()? {
            HeapObject::List(values) | HeapObject::Tuple(values) => Some(values.clone()),
            _ => None,
        },
        _ => None,
    }
}

fn regexp_match_allocation_bytes(
    input: &str,
    units: &[u16],
    found: &CapturedMatch,
) -> Result<u64, RuntimeError> {
    const OBJECT_HEADER: u64 = 16;
    const VALUE_SLOT: u64 = 16;
    const RECORD_FIELD: u64 = 8;

    let string_value_bytes = |range: std::ops::Range<usize>| {
        utf16_range_byte_len(units, range).map(|bytes| VALUE_SLOT.saturating_add(bytes as u64))
    };
    let mut items_bytes = string_value_bytes(found.range.clone())?;
    for range in &found.captures {
        items_bytes = items_bytes.saturating_add(match range {
            Some(range) => string_value_bytes(range.clone())?,
            None => VALUE_SLOT + 1,
        });
    }

    let groups_bytes = if found.named.is_empty() {
        0
    } else {
        let mut bytes = OBJECT_HEADER;
        for (name, range) in &found.named {
            let value_bytes = match range {
                Some(range) => string_value_bytes(range.clone())?,
                None => VALUE_SLOT + 1,
            };
            bytes = bytes
                .saturating_add(RECORD_FIELD)
                .saturating_add(name.len() as u64)
                .saturating_add(value_bytes);
        }
        bytes
    };
    let groups_value_bytes = if found.named.is_empty() {
        VALUE_SLOT + 1
    } else {
        VALUE_SLOT + 8
    };
    let match_bytes = OBJECT_HEADER
        .saturating_add(RECORD_FIELD * 3)
        .saturating_add(items_bytes)
        .saturating_add(VALUE_SLOT + 8)
        .saturating_add(VALUE_SLOT + input.len() as u64)
        .saturating_add(groups_value_bytes);
    Ok(groups_bytes.saturating_add(match_bytes))
}

fn bounded_utf16_input(heap: &Heap, input: &str) -> Result<Vec<u16>, RuntimeError> {
    let units = input.encode_utf16().count();
    let bytes = units
        .checked_mul(std::mem::size_of::<u16>())
        .ok_or_else(|| javascript_string_size_error(usize::MAX))?;
    heap.ensure_additional_logical_bytes(bytes as u64)?;
    let mut output = Vec::new();
    output
        .try_reserve_exact(units)
        .map_err(|_| RuntimeError::MemoryLimitExceeded {
            limit: DEFAULT_HEAP_LOGICAL_BYTE_LIMIT,
            attempted: bytes as u64,
        })?;
    output.extend(input.encode_utf16());
    Ok(output)
}

fn push_utf16_range_bounded(
    heap: &Heap,
    output: &mut Vec<Value>,
    units: &[u16],
    range: std::ops::Range<usize>,
    pending_bytes: &mut u64,
) -> Result<(), RuntimeError> {
    let bytes = utf16_range_byte_len(units, range.clone())? as u64;
    *pending_bytes = pending_bytes.saturating_add(16_u64.saturating_add(bytes));
    heap.ensure_additional_logical_bytes(*pending_bytes)?;
    output
        .try_reserve_exact(1)
        .map_err(|_| RuntimeError::MemoryLimitExceeded {
            limit: DEFAULT_HEAP_LOGICAL_BYTE_LIMIT,
            attempted: u64::MAX,
        })?;
    output.push(utf16_range(units, range)?);
    Ok(())
}

fn push_transient_value_bounded(
    heap: &Heap,
    output: &mut Vec<Value>,
    value: Value,
    value_bytes: u64,
    pending_bytes: &mut u64,
) -> Result<(), RuntimeError> {
    *pending_bytes = pending_bytes.saturating_add(value_bytes);
    heap.ensure_additional_logical_bytes(*pending_bytes)?;
    output
        .try_reserve_exact(1)
        .map_err(|_| RuntimeError::MemoryLimitExceeded {
            limit: DEFAULT_HEAP_LOGICAL_BYTE_LIMIT,
            attempted: u64::MAX,
        })?;
    output.push(value);
    Ok(())
}

fn utf16_range_byte_len(
    units: &[u16],
    range: std::ops::Range<usize>,
) -> Result<usize, RuntimeError> {
    char::decode_utf16(units[range].iter().copied()).try_fold(0usize, |bytes, character| {
        let character = character.map_err(|_| RuntimeError::ValidationFailed {
            reason: "TS_REGEX_LONE_SURROGATE_MATCH_UNSUPPORTED: non-unicode RegExp output contains an unrepresentable lone surrogate".to_string(),
        })?;
        bytes
            .checked_add(character.len_utf8())
            .ok_or_else(|| javascript_string_size_error(usize::MAX))
    })
}

fn utf16_range(units: &[u16], range: std::ops::Range<usize>) -> Result<Value, RuntimeError> {
    utf16_value(units[range].to_vec()).map_err(|_| RuntimeError::ValidationFailed {
        reason: "TS_REGEX_LONE_SURROGATE_MATCH_UNSUPPORTED: non-unicode RegExp output contains an unrepresentable lone surrogate".to_string(),
    })
}

fn to_uint32(number: f64) -> u32 {
    if !number.is_finite() || number == 0.0 {
        return 0;
    }
    number.trunc().rem_euclid(4_294_967_296.0) as u32
}

fn advance_string_index(input: &[u16], index: usize, unicode: bool) -> usize {
    if !unicode || index + 1 >= input.len() {
        return index.saturating_add(1);
    }
    let first = input[index];
    let second = input[index + 1];
    if (0xd800..=0xdbff).contains(&first) && (0xdc00..=0xdfff).contains(&second) {
        index + 2
    } else {
        index + 1
    }
}

fn find_string_matches(
    heap: &Heap,
    input: &[u16],
    needle: &[u16],
    all: bool,
) -> Result<Vec<CapturedMatch>, RuntimeError> {
    let mut matches = Vec::new();
    let mut transient_bytes = 0_u64;
    let mut start = 0;
    loop {
        let found = if needle.is_empty() {
            (start <= input.len()).then_some(start)
        } else {
            input[start..]
                .windows(needle.len())
                .position(|candidate| candidate == needle)
                .map(|offset| start + offset)
        };
        let Some(found) = found else { break };
        transient_bytes =
            transient_bytes.saturating_add(std::mem::size_of::<CapturedMatch>() as u64);
        heap.ensure_additional_logical_bytes(transient_bytes)?;
        matches
            .try_reserve_exact(1)
            .map_err(|_| RuntimeError::MemoryLimitExceeded {
                limit: DEFAULT_HEAP_LOGICAL_BYTE_LIMIT,
                attempted: u64::MAX,
            })?;
        matches.push(CapturedMatch {
            range: found..found + needle.len(),
            captures: Vec::new(),
            named: Vec::new(),
        });
        if !all {
            break;
        }
        start = found + needle.len().max(1);
        if start > input.len() + usize::from(needle.is_empty()) {
            break;
        }
    }
    Ok(matches)
}

fn expand_replacement_checked(
    heap: &Heap,
    output: &mut Vec<u16>,
    output_bytes: &mut usize,
    input: &[u16],
    replacement: &str,
    found: &CapturedMatch,
    lone_surrogate_error: &'static str,
) -> Result<(), RuntimeError> {
    let char_count = replacement.chars().count();
    let transient_bytes = 16_u64
        .saturating_add((char_count as u64).saturating_mul(std::mem::size_of::<char>() as u64));
    heap.ensure_additional_logical_bytes(transient_bytes)?;
    let mut chars = Vec::new();
    chars
        .try_reserve_exact(char_count)
        .map_err(|_| RuntimeError::MemoryLimitExceeded {
            limit: DEFAULT_HEAP_LOGICAL_BYTE_LIMIT,
            attempted: transient_bytes,
        })?;
    chars.extend(replacement.chars());
    let mut index = 0;
    while index < chars.len() {
        if chars[index] != '$' || index + 1 == chars.len() {
            let mut encoded = [0_u16; 2];
            let encoded = chars[index].encode_utf16(&mut encoded);
            append_utf16_checked(heap, output, output_bytes, encoded, lone_surrogate_error)?;
            index += 1;
            continue;
        }
        match chars[index + 1] {
            '$' => {
                append_utf16_checked(
                    heap,
                    output,
                    output_bytes,
                    &['$' as u16],
                    lone_surrogate_error,
                )?;
                index += 2;
            }
            '&' => {
                append_utf16_checked(
                    heap,
                    output,
                    output_bytes,
                    &input[found.range.clone()],
                    lone_surrogate_error,
                )?;
                index += 2;
            }
            '`' => {
                append_utf16_checked(
                    heap,
                    output,
                    output_bytes,
                    &input[..found.range.start],
                    lone_surrogate_error,
                )?;
                index += 2;
            }
            '\'' => {
                append_utf16_checked(
                    heap,
                    output,
                    output_bytes,
                    &input[found.range.end..],
                    lone_surrogate_error,
                )?;
                index += 2;
            }
            '<' if !found.named.is_empty() => {
                let Some(relative_end) = chars[index + 2..].iter().position(|ch| *ch == '>') else {
                    append_utf16_checked(
                        heap,
                        output,
                        output_bytes,
                        &['$' as u16],
                        lone_surrogate_error,
                    )?;
                    index += 1;
                    continue;
                };
                let end = index + 2 + relative_end;
                let name = chars[index + 2..end].iter().collect::<String>();
                if let Some((_, range)) =
                    found.named.iter().find(|(candidate, _)| *candidate == name)
                    && let Some(range) = range
                {
                    append_utf16_checked(
                        heap,
                        output,
                        output_bytes,
                        &input[range.clone()],
                        lone_surrogate_error,
                    )?;
                }
                index = end + 1;
            }
            digit if digit.is_ascii_digit() && digit != '0' => {
                let first = digit.to_digit(10).unwrap() as usize;
                let second = chars
                    .get(index + 2)
                    .and_then(|digit| digit.to_digit(10))
                    .map(|digit| digit as usize);
                let combined = second.map(|digit| first * 10 + digit);
                let (capture, consumed) =
                    if combined.is_some_and(|value| value <= found.captures.len()) {
                        (combined.unwrap(), 3)
                    } else if first <= found.captures.len() {
                        (first, 2)
                    } else {
                        append_utf16_checked(
                            heap,
                            output,
                            output_bytes,
                            &['$' as u16],
                            lone_surrogate_error,
                        )?;
                        index += 1;
                        continue;
                    };
                if let Some(range) = &found.captures[capture - 1] {
                    append_utf16_checked(
                        heap,
                        output,
                        output_bytes,
                        &input[range.clone()],
                        lone_surrogate_error,
                    )?;
                }
                index += consumed;
            }
            _ => {
                append_utf16_checked(
                    heap,
                    output,
                    output_bytes,
                    &['$' as u16],
                    lone_surrogate_error,
                )?;
                index += 1;
            }
        }
    }
    Ok(())
}

fn append_utf16_checked(
    heap: &Heap,
    output: &mut Vec<u16>,
    output_bytes: &mut usize,
    units: &[u16],
    lone_surrogate_error: &'static str,
) -> Result<(), RuntimeError> {
    let additional_bytes =
        char::decode_utf16(units.iter().copied()).try_fold(0_usize, |bytes, character| {
            let character = character.map_err(|_| RuntimeError::ValidationFailed {
                reason: lone_surrogate_error.to_string(),
            })?;
            bytes
                .checked_add(character.len_utf8())
                .ok_or_else(|| javascript_string_size_error(usize::MAX))
        })?;
    let attempted = output_bytes
        .checked_add(additional_bytes)
        .ok_or_else(|| javascript_string_size_error(usize::MAX))?;
    ensure_javascript_string_size(attempted)?;
    let attempted_units = output
        .len()
        .checked_add(units.len())
        .ok_or_else(|| javascript_string_size_error(usize::MAX))?;
    let transient_bytes = (attempted_units as u64)
        .saturating_mul(std::mem::size_of::<u16>() as u64)
        .max(attempted as u64)
        .saturating_add(16);
    heap.ensure_additional_logical_bytes(transient_bytes)?;
    output
        .try_reserve_exact(units.len())
        .map_err(|_| RuntimeError::MemoryLimitExceeded {
            limit: DEFAULT_HEAP_LOGICAL_BYTE_LIMIT,
            attempted: transient_bytes,
        })?;
    output.extend_from_slice(units);
    *output_bytes = attempted;
    Ok(())
}

fn lone_surrogate_output_error(regexp: bool) -> &'static str {
    if regexp {
        "TS_REGEX_LONE_SURROGATE_MATCH_UNSUPPORTED: non-unicode RegExp output contains an unrepresentable lone surrogate"
    } else {
        "TS_LONE_SURROGATE_UNSUPPORTED: result is not representable"
    }
}

fn node_regexp_error_detail<'a>(pattern: &str, regress_detail: &'a str) -> &'a str {
    if regress_detail == "Unbalanced parenthesis" && has_unclosed_group(pattern) {
        "Unterminated group"
    } else {
        regress_detail
    }
}

fn has_unclosed_group(pattern: &str) -> bool {
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
            '(' if !in_class => depth += 1,
            ')' if !in_class => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    depth != 0
}
