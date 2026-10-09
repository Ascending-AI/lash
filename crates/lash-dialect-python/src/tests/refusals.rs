//! What the dialect does not have it refuses, by code.

use lash_kernel_dialect::DiagnosticKind;

use super::machine;
use crate::Code;

/// One source per refusal, with the code it is refused under.
const REFUSED: &[(&str, Code)] = &[
    ("def f(:\n", Code::Syntax),
    ("print(nowhere)\n", Code::UnknownName),
    ("len(1, 2)\n", Code::Arguments),
    ("def f():\n    await echo(1)\n", Code::AwaitOutsideAsync),
    ("return 1\n", Code::ReturnOutsideFunction),
    ("x = echo('a')\n", Code::CoroutineNotAwaited),
    (
        "async def f():\n    return 1\nx = f()\n",
        Code::CoroutineNotAwaited,
    ),
    ("class Point:\n    x = 1\n", Code::ClassUnsupported),
    ("def gen():\n    yield 1\n", Code::GeneratorUnsupported),
    ("with open('f') as f:\n    pass\n", Code::WithUnsupported),
    (
        "match 1:\n    case 1:\n        pass\n",
        Code::MatchUnsupported,
    ),
    ("@wrap\ndef f():\n    pass\n", Code::DecoratorUnsupported),
    ("import os\n", Code::ImportUnsupported),
    ("from asyncio import gather\n", Code::ImportUnsupported),
    ("import re\n", Code::RegexUnsupported),
    ("from re import match\n", Code::RegexUnsupported),
    ("def f(*args):\n    pass\n", Code::StarUnsupported),
    ("def f(a, *, b):\n    pass\n", Code::StarUnsupported),
    ("first, *rest = [1, 2]\n", Code::StarUnsupported),
    ("xs = [1]\nprint(*xs)\n", Code::StarUnsupported),
    ("fs = [lambda a: a]\nfs[0](a=1)\n", Code::KeywordCallDynamic),
    ("x = (1).real\n", Code::AttributeUnsupported),
    ("x = 'a'.title()\n", Code::MethodUnsupported),
    ("x = '{}'.format(1)\n", Code::MethodUnsupported),
    ("f = len\n", Code::BuiltinAsValue),
    ("x = list(map(str, [1]))\n", Code::BuiltinUnsupported),
    ("x = 1 << 2\n", Code::OperatorUnsupported),
    ("x = ~1\n", Code::OperatorUnsupported),
    ("x = '%d' % 1\n", Code::OperatorUnsupported),
    ("x = f'{1.5:g}'\n", Code::FormatSpecUnsupported),
    ("w = 5\nx = f'{1:{w}}'\n", Code::FormatSpecUnsupported),
    ("x = 1\ny = f'{x=}'\n", Code::FormatSpecUnsupported),
    (
        "import asyncio\nawait asyncio.wait_for(echo(1), 1)\n",
        Code::AsyncUnsupported,
    ),
    (
        "import asyncio\nx = 1\nt = asyncio.create_task(x)\n",
        Code::AsyncUnsupported,
    ),
    ("x = 1\ndel x\n", Code::DeleteUnsupported),
    ("xs = [1, 2]\nxs[0:1] = [3]\n", Code::TargetUnsupported),
    ("x = [1]\nx.field = 1\n", Code::TargetUnsupported),
    ("kind = ValueError\n", Code::ExceptionClass),
    (
        "try:\n    pass\nexcept unknown_class:\n    pass\n",
        Code::ExceptionClass,
    ),
    (
        "x = 1\ndef outer():\n    x = 2\n    def inner():\n        global x\n        x = 3\n",
        Code::GlobalShadowed,
    ),
    ("x = 1j\n", Code::LiteralUnsupported),
    ("x = b'bytes'\n", Code::LiteralUnsupported),
    ("x = (y := 1)\n", Code::SyntaxUnsupported),
];

#[test]
fn each_refusal_carries_its_code() {
    for (source, code) in REFUSED {
        match machine::lower(source) {
            Ok(_) => panic!("lowered, where {} was expected:\n{source}", code.as_str()),
            Err(diagnostic) => {
                assert_eq!(diagnostic.code, code.as_str(), "{diagnostic}\n{source}");
                assert_eq!(diagnostic.kind, code.kind(), "{source}");
                assert!(
                    diagnostic.span.is_some() || *code == Code::Syntax,
                    "{} names no place in:\n{source}",
                    code.as_str()
                );
                if code.kind() == DiagnosticKind::Refusal {
                    assert!(
                        !diagnostic.repairs.is_empty(),
                        "{} says nothing to write instead:\n{source}",
                        code.as_str()
                    );
                }
            }
        }
    }
}

/// A program nested past the kernel's limit is refused, not lowered to a
/// document the kernel would refuse later.
#[test]
fn nesting_past_the_kernel_limit_is_refused() {
    let mut source = String::new();
    for depth in 0..40 {
        source.push_str(&format!("{}if True:\n", "    ".repeat(depth)));
    }
    source.push_str(&format!("{}pass\n", "    ".repeat(40)));
    let diagnostic = machine::lower(&source).expect_err("forty nested blocks");
    assert_eq!(
        diagnostic.code,
        Code::InvalidDocument.as_str(),
        "{diagnostic}"
    );
}

/// An expression nested past what the lowerer walks is refused before it
/// can exhaust the stack.
#[test]
fn an_expression_nested_too_deep_is_refused() {
    let source = format!("x = {}1{}\n", "(".repeat(400), ")".repeat(400));
    let nested = format!("x = {}1{}\n", "[".repeat(400), "]".repeat(400));
    assert!(
        machine::lower(&source).is_ok(),
        "parentheses are not nesting"
    );
    let diagnostic = machine::lower(&nested).expect_err("four hundred nested lists");
    assert_eq!(diagnostic.code, Code::TooDeep.as_str(), "{diagnostic}");
}

#[test]
fn a_source_past_the_size_limit_is_refused() {
    let source = "#".repeat(crate::MAX_SOURCE_BYTES + 1);
    let diagnostic = machine::lower(&source).expect_err("a source of over a megabyte");
    assert_eq!(diagnostic.code, Code::SourceTooLarge.as_str());
}

/// Without the dialect's helpers installed the front end names the one it
/// needs; it does not emit a call to a function the embedder lacks.
#[test]
fn a_missing_helper_is_named() {
    let library = lash_kernel_dialect::NamedLibrary::new();
    let effects = std::collections::BTreeMap::new();
    let bindings = std::collections::BTreeSet::new();
    let environment = lash_kernel_dialect::Environment {
        library: &library,
        effects: &effects,
        bindings: &bindings,
    };
    let diagnostic = crate::lower("print(1)\n", &environment).expect_err("an empty library");
    assert_eq!(diagnostic.code, Code::LibraryMissing.as_str());
    assert!(diagnostic.message.contains("py.show"), "{diagnostic}");
}
