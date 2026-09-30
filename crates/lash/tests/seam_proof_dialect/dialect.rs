//! A seam proof, not a language: the smallest code-mode dialect that shows a
//! second front end plugs into the host through the public `Dialect` seam.
//! The host adapter selects the compiled worker that owns its source parser.
//! Both live only in this test target.
//!
//! One statement per line, two forms:
//!
//! ```text
//! take NAME from MODULE.OPERATION WITH {json object}
//! give VALUE        where VALUE is NAME, NAME.FIELD or a JSON literal
//! ```

use lash::rlm::{
    CellTags, Dialect, DialectPromptVocabulary, DialectRefusal, DialectRefusalKind,
    ExecutionSectionRequest, ResolvedToolBinding, RlmChannel, ShapeNotation,
};

pub const LANGUAGE_ID: &str = "seam-proof";

pub const CELL_TAGS: CellTags = CellTags {
    open: "<seam>",
    close: "</seam>",
};

pub struct SeamProofDialect;

impl Dialect for SeamProofDialect {
    fn language_id(&self) -> &'static str {
        LANGUAGE_ID
    }

    fn worker_service(&self) -> lash::rlm::WorkerService {
        let entry = lash::rlm::WorkerEntry::helper(super::worker_executable());
        lash::rlm::WorkerService::new(lash::rlm::WorkerPoolConfig::standard(entry))
    }

    fn tool_call_path(&self, binding: &ResolvedToolBinding) -> Result<String, DialectRefusal> {
        let path = binding.call_path();
        if path.split('.').all(is_name) {
            Ok(path)
        } else {
            Err(DialectRefusal {
                kind: DialectRefusalKind::UnaddressableToolPath,
                message: format!("`{path}` is not a seam-proof call path"),
            })
        }
    }

    fn tool_signature(
        &self,
        call_path: &str,
        input_schema: &serde_json::Value,
        output_schema: &serde_json::Value,
    ) -> String {
        format!(
            "{call_path} WITH {} GIVES {}",
            schema_notation(input_schema),
            schema_notation(output_schema)
        )
    }

    fn render_tool_example(&self, _authored: &str) -> Option<String> {
        None
    }

    fn prompt_vocabulary(&self) -> DialectPromptVocabulary {
        DialectPromptVocabulary {
            language_name: "Seam proof",
            execution_title: "Seam proof execution",
            cell_tags: CELL_TAGS,
            cell_noun: "cell",
            history_type: "Log",
            print_call: "give",
            print_statement_prefix: "give ",
            print_statement_suffix: "",
            finish_name: "give",
            finish_statement: "give VALUE",
            finish_null_statement: "give null",
            continue_as_call: "take r from control.continue_as WITH {...}",
            continue_as_example: r#"take r from control.continue_as WITH {"task": "go on"}"#,
            field_miss_rule: "Use only the field names listed below.",
            shape_notation: NOTATION,
        }
    }

    fn history_item_definition(&self, _images: bool) -> Vec<String> {
        vec!["shape Log is seq of Rec".to_string()]
    }

    fn render_execution_section(&self, request: ExecutionSectionRequest<'_>) -> String {
        let transport = match request.channel {
            RlmChannel::Cell => {
                "Write the program between standalone `<seam>` and `</seam>` lines."
            }
            RlmChannel::NativeTool => "Send the program as the `code` of one `execute_code` call.",
        };
        format!(
            "{transport} One statement per line. `take NAME from MODULE.OPERATION WITH {{json}}` calls a tool and names its result; `give VALUE` ends the turn with VALUE, a name, `name.field` or a JSON literal.\n\n### Tools\n\n{}",
            request.tools
        )
    }
}

const NOTATION: ShapeNotation = ShapeNotation {
    any: "Anything",
    null: "Nothing",
    bool: "Flag",
    int: "Whole",
    float: "Real",
    str: "Text",
    record: "Rec",
    list_open: "seq of ",
    list_close: "",
    union_separator: " or ",
    definition_keyword: "shape ",
    definition_assign: " is ",
    record_open: "(",
    field_indent: "  ",
    field_separator: " -> ",
    field_terminator: ";",
    record_close: ")",
};

fn is_name(text: &str) -> bool {
    text.chars()
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic())
        && text
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_')
}

fn schema_notation(schema: &serde_json::Value) -> &'static str {
    match schema.get("type").and_then(serde_json::Value::as_str) {
        Some("string") => NOTATION.str,
        Some("integer") => NOTATION.int,
        Some("number") => NOTATION.float,
        Some("boolean") => NOTATION.bool,
        Some("object") => NOTATION.record,
        Some("null") => NOTATION.null,
        _ => NOTATION.any,
    }
}
