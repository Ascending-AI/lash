//! A seam proof, not a language: the smallest code-mode dialect that shows a
//! second front end plugs into the host through the public dialect seam:
//! [`cell_dialect`] names the package the test worker installs and words
//! its prompts, and [`worker_service`] selects that worker.
//! Both live only in this test target.
//!
//! One statement per line, two forms:
//!
//! ```text
//! take NAME from MODULE.OPERATION WITH {json object}
//! give VALUE        where VALUE is NAME, NAME.FIELD or a JSON literal
//! ```

use lash::rlm::{
    CellDialect, CellTags, DialectPromptVocabulary, DialectPrompts, DialectRefusal,
    DialectRefusalKind, ExecutionSection, ExecutionSectionRequest, ResolvedToolBinding, RlmChannel,
    SchemaShape,
};

pub const LANGUAGE_ID: &str = "seam-proof";

pub const CELL_TAGS: CellTags = CellTags {
    open: "<seam>",
    close: "</seam>",
};

/// The seam-proof dialect as a host selects it.
pub fn cell_dialect() -> CellDialect {
    CellDialect::new(
        LANGUAGE_ID,
        lash::workflow::document::NumberPolicy::Float,
        std::sync::Arc::new(SeamProofPrompts),
    )
}

/// The worker pool whose entry installs the seam-proof package.
pub fn worker_service() -> lash::vm::WorkerService {
    let entry = lash::vm::WorkerEntry::helper(super::worker_executable());
    lash::vm::WorkerService::new(lash::vm::WorkerPoolConfig::standard(entry))
}

struct SeamProofPrompts;

impl DialectPrompts for SeamProofPrompts {
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
            history_type: "list[LogItem]",
            history_item_name: "LogItem",
            print_call: "give",
            print_statement_prefix: "give ",
            print_statement_suffix: "",
            finish_call: "give VALUE",
            continue_as_call: "take unused from control.continue_as WITH {...}",
            continue_as_example: r#"take unused from control.continue_as WITH {"task": "go on"}"#,
            field_miss_rule: "Use only the field names listed below.",
            not_carried_repair: "Take the value again in this cell.",
            unjoined_task_repair: "Take every result before the cell ends.",
        }
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
