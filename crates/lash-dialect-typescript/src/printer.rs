//! Source spelling shared by ordinary lowering and readable export.

pub(crate) fn legal_identifier(name: &str) -> bool {
    property_identifier(name)
        && !name.starts_with("helper_")
        && !name.starts_with("kernel_name_")
        && !matches!(
            name,
            "k" | "Object"
                | "projection"
                | "main"
                | "declared"
                | "finish"
                | "failure"
                | "sleep"
                | "join"
                | "spawn"
                | "cancel"
                | "yieldTask"
                | "Promise"
                | "console"
                | "Math"
                | "Date"
                | "Set"
                | "Map"
                | "Uint8Array"
                | "KernelType"
                | "undefined"
                | "NaN"
                | "Infinity"
                | "eval"
                | "arguments"
                | "await"
                | "break"
                | "case"
                | "catch"
                | "class"
                | "const"
                | "continue"
                | "debugger"
                | "default"
                | "delete"
                | "do"
                | "else"
                | "enum"
                | "export"
                | "extends"
                | "false"
                | "finally"
                | "for"
                | "function"
                | "if"
                | "implements"
                | "import"
                | "in"
                | "instanceof"
                | "interface"
                | "let"
                | "new"
                | "null"
                | "package"
                | "private"
                | "protected"
                | "public"
                | "return"
                | "static"
                | "super"
                | "switch"
                | "this"
                | "throw"
                | "true"
                | "try"
                | "typeof"
                | "var"
                | "void"
                | "while"
                | "with"
                | "yield"
        )
}

/// Property names have no binding-keyword or alias-namespace restrictions.
pub(crate) fn property_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    swc_ecma_ast::Ident::is_valid_start(first) && chars.all(swc_ecma_ast::Ident::is_valid_continue)
}
