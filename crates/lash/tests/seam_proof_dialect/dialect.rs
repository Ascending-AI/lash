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
    ExecutionSection, ExecutionSectionRequest, ResolvedToolBinding, RlmChannel, SchemaShape,
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

    fn tool_signature(&self, call_path: &str, input: &SchemaShape, output: &SchemaShape) -> String {
        format!(
            "{call_path} WITH {} GIVES {}",
            self.schema_type(input),
            self.schema_type(output)
        )
    }

    fn schema_type(&self, shape: &SchemaShape) -> String {
        shape.compact_type()
    }

    fn schema_definition(&self, name: &str, shape: &SchemaShape) -> String {
        format!("shape {name} is {}", self.schema_type(shape))
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
        }
    }

    fn history_item_definition(&self, _images: bool) -> Vec<String> {
        vec!["shape Log is list[record]".to_string()]
    }

    fn render_execution_section(&self, request: ExecutionSectionRequest<'_>) -> ExecutionSection {
        let transport = match request.channel {
            RlmChannel::Cell => {
                "Write the program between standalone `<seam>` and `</seam>` lines."
            }
            RlmChannel::NativeTool => "Send the program as the `code` of one `execute_code` call.",
        };
        ExecutionSection {
            prose: format!(
                "{transport} One statement per line. `take NAME from MODULE.OPERATION WITH {{json}}` calls a tool and names its result; `give VALUE` ends the turn with VALUE, a name, `name.field` or a JSON literal."
            ),
            declarations: format!("### Tools\n\n{}", request.tools),
        }
    }
}

fn is_name(text: &str) -> bool {
    text.chars()
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic())
        && text
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_')
}
