//! The plan-call ABI (owner: L3 after handoff H0, module-map §3).
//!
//! A [`lash_kernel_doc::plan::HelperPlan`] runs where a native would
//! answer, through the machine's own primitives
//! ([`lash_kernel_doc::plan::PlanContext`]). Its frame's saved state is the
//! body's coordinate, so a frame a plan parked restores into the body and
//! back.
//!
//! The skeleton installs no plan: every call a plan could answer runs the
//! definition's body, which is the definition's meaning.
