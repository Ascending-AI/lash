mod actions;
mod cell;
mod driver;
mod finish;
mod prompt;
mod state;
#[cfg(test)]
mod tests;

pub use cell::contains_lashlang_cell;
pub(crate) use cell::{
    CellExtractionError,
    project_visible_assistant_prose_with_tags as project_visible_assistant_prose_for_dialect,
};
pub use driver::RlmDriver;
#[cfg(feature = "testing")]
pub use driver::project_conformance_messages_through_rlm_history;
pub use prompt::{RlmPromptFeatures, rlm_execution_section_for_host_environment};

pub(crate) use finish::turn_limit_final_message;
