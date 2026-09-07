//! Fuzzes the Lashlang front end on arbitrary source text: lexer, program
//! parser, and the expression/type sub-grammars.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(source) = std::str::from_utf8(data) {
        let _ = lashlang::lex(source);
        let _ = lashlang::parse(source);
        let _ = lashlang::parse_expression(source);
        let _ = lashlang::parse_type_expression(source);
    }
});
