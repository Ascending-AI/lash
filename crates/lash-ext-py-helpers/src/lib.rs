//! Python helper definitions and value catalogues, without a source frontend.

pub mod methods;
pub mod package;
pub mod pure_modules;

/// Installed helpers use the same argument metadata and binder as authored defs.
pub fn helper_value(name: &str, params: &[(&str, u8, bool)], body: &str) -> String {
    let metadata = params
        .iter()
        .map(|(name, mode, optional)| format!("({name:?}, {mode}, {optional})"))
        .collect::<Vec<_>>()
        .join(", ");
    let metadata = format!("[{metadata}]");
    let bindings = params
        .iter()
        .enumerate()
        .map(|(index, _)| format!("let argument{index} = slots[{index}]\n"))
        .collect::<String>();
    format!(
        "(\"py.function\", fn(args, kwargs, active) {{ let slots = invoke py.bind({name:?}, {metadata}, args, kwargs, false)\n{bindings}{body} }}, {metadata}, {name:?}, null)"
    )
}
