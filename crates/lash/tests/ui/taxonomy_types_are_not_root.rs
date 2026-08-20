fn main() {
    // Observation, trigger, process, and tool-state taxonomy types
    // are single-homed in their domain modules (`lash::observe`,
    // `lash::triggers`, `lash::process`, `lash::tools`), not at the root.
    let _: Option<lash::SessionObservation> = None;
    let _: Option<lash::SessionCursor> = None;
    let _: Option<lash::SessionResume> = None;
    let _: Option<lash::TriggerEvent> = None;
    let _: Option<lash::TriggerRegistration> = None;
    let _: Option<lash::ProcessHandleView> = None;
    let _: Option<lash::ToolState> = None;
    let _ = lash::empty_trigger_source_key("ui.button.pressed");
}
