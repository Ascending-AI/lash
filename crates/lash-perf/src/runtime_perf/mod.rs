pub(crate) mod compare_history;
mod duration_trend;
pub(crate) use duration_trend::{ExperimentIdentity, history_run_id};
mod kernel_history;
pub use kernel_history::record_kernel_receipt;
mod replay_history;
pub use replay_history::record_replay_receipt;
mod harness;
mod measurement;
pub(crate) mod openai_compat;
mod plugin_stack;
mod prompt;
pub(crate) mod providers;
mod report;
mod scenarios;
mod smoke;
mod store;

pub use duration_trend::{
    export_duration_history_csv, record_counted_sample, run_duration_trend_cli,
};
pub(crate) use measurement::{HighTrafficMix, HighTrafficPopulation, high_traffic_prompt};
pub use report::{BudgetEnforcement, RuntimePerfRun, run_cli};

pub mod http_population;
