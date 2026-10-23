//! What the dialect does not have it refuses, by code.

use lash_kernel_dialect::DiagnosticKind;

use super::machine;
use crate::Code;

/// One source per refusal, with the code it is refused under.
const REFUSED: &[(&str, Code)] = &[
    ("def f(:\n", Code::Syntax),
    ("print(nowhere)\n", Code::UnknownName),
    ("len(1, 2)\n", Code::Arguments),
    (
        "def f():\n    await host.echo(1)\n",
        Code::AwaitOutsideAsync,
    ),
    ("return 1\n", Code::ReturnOutsideFunction),
    ("x = host.echo('a')\n", Code::CoroutineNotAwaited),
    (
        "async def f():\n    return 1\nx = f()\n",
        Code::CoroutineNotAwaited,
    ),
    ("class Point:\n    x = 1\n", Code::ClassUnsupported),
    ("def gen():\n    yield 1\n", Code::GeneratorUnsupported),
    ("def f(*args):\n    pass\n", Code::StarUnsupported),
    ("def f(a, *, b):\n    pass\n", Code::StarUnsupported),
    ("first, *rest = [1, 2]\n", Code::StarUnsupported),
    ("xs = [1]\nprint(*xs)\n", Code::StarUnsupported),
    ("fs = [lambda a: a]\nfs[0](a=1)\n", Code::KeywordCallDynamic),
    ("f = len\n", Code::BuiltinAsValue),
    ("x = list(map(str, [1]))\n", Code::BuiltinUnsupported),
    (
        "import asyncio\nawait asyncio.wait_for(host.echo(1), 1)\n",
        Code::AsyncUnsupported,
    ),
    (
        "import asyncio\nx = 1\nt = asyncio.create_task(x)\n",
        Code::AsyncUnsupported,
    ),
    ("x = [1]\nx.field = 1\n", Code::TargetUnsupported),
    ("kind = ValueError\n", Code::ExceptionClass),
    (
        "try:\n    pass\nexcept unknown_class:\n    pass\n",
        Code::ExceptionClass,
    ),
    ("x = 1j\n", Code::LiteralUnsupported),
    (
        "def f():\n    pass\nx = f.__globals__\n",
        Code::AttributeUnsupported,
    ),
    ("x = f'{1:n}'\n", Code::FormatSpecUnsupported),
];

/// FIG-5777: repair forms retain the submitted callee and its operands.
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
    let environment_namespaces = crate::tests::linked::namespaces(&effects, &Default::default())
        .expect("the test's effects are operations");
    let environment = lash_kernel_dialect::Environment::new(&library, &environment_namespaces);
    let diagnostic = crate::lower(
        "print(1)\n",
        &environment,
        lash_kernel_dialect::Mode::Cell(lash_kernel_dialect::CellEnvironment {
            bindings: &bindings,
            functions: &std::collections::BTreeMap::new(),
        }),
    )
    .expect_err("an empty library");
    assert_eq!(diagnostic.code, Code::LibraryMissing.as_str());
    assert!(diagnostic.message.contains("py.show"), "{diagnostic}");
}

/// FIG-5777: every accepted idiom offered by the refusal diagnostics has
/// a lowering witness. Concrete repairs are checked against their emitting
/// site as well, so replacing them with generic prose cannot satisfy the law.
#[test]
fn every_advertised_repair_form_lowers() {
    use Code::*;
    let examples = [
        (
            SavedFunctionUnusable,
            "def invoice(total):\n    return total\ninvoice(42)",
        ),
        (ShadowsBuiltin, "len_ = 1\nlen_\n"),
        (
            CoroutineNotAwaited,
            "import asyncio\nasync def load_invoice(invoice):\n    return invoice\nawait load_invoice(42)\njob = asyncio.create_task(load_invoice(43))\nawait job\nawait host.echo(1)\nawait asyncio.sleep(0)\nawait asyncio.gather(host.echo(2))",
        ),
        (
            ClassUnsupported,
            "invoice = {'total': 1}\ndef total(invoice):\n    return invoice['total']\nclass InvoiceError(Exception):\n    pass\nraise InvoiceError()",
        ),
        (
            GeneratorUnsupported,
            "def collect():\n    return [1, 2]\ncollect()",
        ),
        (
            MatchUnsupported,
            "invoice = 1\nif invoice == 1:\n    print(invoice)\nelif invoice == 2:\n    print(2)",
        ),
        (
            ImportUnsupported,
            "import asyncio\nawait asyncio.sleep(0)\nawait host.echo(1)",
        ),
        (
            RegexUnsupported,
            "invoice = 'paid,open'\ninvoice.find('paid')\ninvoice.split(',')\ninvoice.startswith('paid')\ninvoice.replace('paid', 'done')",
        ),
        (
            StarUnsupported,
            "def collect(items, options):\n    return items\nxs = [1, 2]\nfirst, rest = xs[0], xs[1:]\nys = xs + [3]\nfields = {'total': 1}\nfields.update({'paid': True})\ncollect(xs, fields)\nprint(first, rest)\nimport asyncio\ntasks = [asyncio.create_task(host.echo(1))]\nawait asyncio.gather(*tasks)",
        ),
        (KeywordCallDynamic, "fs = [lambda a: a]\nfs[0](1)"),
        (
            AttributeUnsupported,
            "invoice = {'total': 1}\ninvoice['total']\ntry:\n    raise ValueError('bad')\nexcept ValueError as error:\n    print(error.args, error.__cause__)",
        ),
        (
            MethodUnsupported,
            "invoice = 1\nf'{invoice}'\nitems = [invoice]\nitems.append(2)",
        ),
        (
            BuiltinAsValue,
            "def length(items):\n    return len(items)\nlength([1])",
        ),
        (
            BuiltinUnsupported,
            "xs = [1, 2]\n[str(item) for item in xs]\n[item for item in xs if item]\n2 ** 3\nf'{xs}'\ntype(xs).__name__\nisinstance(xs, list)",
        ),
        (
            OperatorUnsupported,
            "invoice = 2\nshift = 1\ninvoice * (2 ** shift)\ninvoice // (2 ** shift)\n-invoice - 1\nf'{invoice}'",
        ),
        (
            FormatSpecUnsupported,
            "invoice = 42\nf'{invoice:d}'\nf'{invoice:f}'\nf'{invoice:e}'\nf'{invoice:%}'\nf'{invoice:x}'\nf'{invoice:o}'\nf'{invoice:b}'\nf'{invoice:08d}'\nf'{invoice:10.2f}'",
        ),
        (
            AsyncUnsupported,
            "import asyncio\nasync def load_invoice():\n    return await host.echo(1)\nasyncio.run(load_invoice())\nfor invoice in [1, 2]:\n    await host.echo(invoice)\njob = asyncio.create_task(load_invoice())\nawait job",
        ),
        (
            TargetUnsupported,
            "invoices = [1, 2]\ninvoices = [3] + invoices[1:]\ninvoice = {'total': 1}\ninvoice['total'] = 2\ninvoice['total'] += 1",
        ),
        (
            ExceptionClass,
            "class InvoiceError(Exception):\n    pass\ntry:\n    raise InvoiceError()\nexcept InvoiceError:\n    pass",
        ),
        (
            LiteralUnsupported,
            "text = 'invoice'\nmissing = None\nreal, imaginary = 1.0, 2.0\nlarge = 123456789012345678901234567890",
        ),
        (
            TooDeep,
            "subtotal = 1 + 2\ninvoice = subtotal + 3\nprint(invoice)",
        ),
        (
            SyntaxUnsupported,
            "invoice = 1\nf'{invoice}'\nxs = [1, 2]\nstart, stop = 0, 1\nxs[start:stop]\ntry:\n    raise ValueError('bad')\nexcept ValueError:\n    pass",
        ),
    ];
    for code in Code::ALL {
        if code.kind() == DiagnosticKind::Refusal
            && !matches!(code, SourceTooLarge | InvalidDocument | LibraryMissing)
        {
            assert!(
                examples.iter().any(|(covered, _)| covered == code),
                "{} advertises a repair without a witness",
                code.as_str()
            );
        }
    }
    for (code, source) in examples {
        let accepted = machine::lower(source).map(|_| ());
        accepted.unwrap_or_else(|error| {
            panic!(
                "{} suggests an unaccepted idiom: {error}\n{source}",
                code.as_str()
            )
        });
    }
    let sites = [
        (
            "async def load_invoice(invoice_id):\n    return invoice_id\ninvoice_number = 42\n",
            "load_invoice(invoice_number)",
            CoroutineNotAwaited,
            "await load_invoice(invoice_number)",
        ),
        (
            "invoice_number = 42\n",
            "host.echo(invoice_number)",
            CoroutineNotAwaited,
            "await host.echo(invoice_number)",
        ),
        (
            "import asyncio\ndelay = 0\n",
            "asyncio.sleep(delay)",
            CoroutineNotAwaited,
            "await asyncio.sleep(delay)",
        ),
        (
            "import asyncio\n",
            "asyncio.gather(host.echo(1), host.echo(2))",
            CoroutineNotAwaited,
            "await asyncio.gather(host.echo(1), host.echo(2))",
        ),
        (
            "invoices = [1, 2]\n",
            "map(str, invoices)",
            BuiltinUnsupported,
            "[(str)(item) for item in (invoices)]",
        ),
        (
            "item = 9\ninvoices = [1, 2]\n",
            "map(lambda n: n + item, invoices)",
            BuiltinUnsupported,
            "[(lambda n: n + item)(item_) for item_ in (invoices)]",
        ),
        (
            "invoices = [1, 2]\n",
            "filter(None, invoices)",
            BuiltinUnsupported,
            "[item for item in (invoices) if item]",
        ),
        (
            "invoices = [1, 2]\n",
            "filter(lambda n: n > 1, invoices)",
            BuiltinUnsupported,
            "[item for item in (invoices) if (lambda n: n > 1)(item)]",
        ),
        (
            "invoice_number = 42\nexponent = 2\n",
            "pow(invoice_number + 1, exponent)",
            BuiltinUnsupported,
            "(invoice_number + 1) ** (exponent)",
        ),
        (
            "invoice_number = 42\n",
            "type(invoice_number)",
            BuiltinUnsupported,
            "type(invoice_number).__name__",
        ),
        (
            "invoice = {'total': 1}\n",
            "invoice.total = 2",
            TargetUnsupported,
            "(invoice)[\"total\"] = 2",
        ),
        (
            "fs = [lambda a: a]\ninvoice_number = 42\n",
            "fs[0](a=invoice_number)",
            KeywordCallDynamic,
            "fs[0](invoice_number)",
        ),
    ];
    for (prelude, refused, code, form) in sites {
        let diagnostic =
            machine::lower(&format!("{prelude}{refused}\n")).expect_err("the construct is refused");
        assert_eq!(diagnostic.code, code.as_str(), "{diagnostic}");
        assert!(
            diagnostic
                .repairs
                .iter()
                .any(|repair| repair.contains(form)),
            "{} must offer {form}: {diagnostic}",
            code.as_str()
        );
        machine::lower(&format!("{prelude}{form}\n"))
            .unwrap_or_else(|error| panic!("the offered form must lower: {error}\n{form}"));
    }
}

/// SOURCE-ADMISSION-POLICY: a host can raise source bytes without changing the native nesting ceiling.
#[test]
fn a_host_source_policy_allows_a_larger_python_cell() {
    let source = format!("#{}", "x".repeat(crate::MAX_SOURCE_BYTES));
    let environment_namespaces =
        crate::tests::linked::namespaces(&Default::default(), &Default::default())
            .expect("the test's effects are operations");
    let environment =
        lash_kernel_dialect::Environment::new(machine::library(), &environment_namespaces);
    let cell = lash_kernel_dialect::Mode::Cell(lash_kernel_dialect::CellEnvironment::first());
    assert!(crate::lower(&source, &environment, cell).is_err());
    let limits = crate::SourceLimits::new(source.len(), 96).unwrap();
    crate::lower_with_limits(&source, &environment, cell, limits).unwrap();
}
