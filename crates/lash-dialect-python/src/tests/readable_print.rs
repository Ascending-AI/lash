//! READABLE-FALLBACK (FIG-6953): a function no authored source lowers to
//! prints in kernel form, opened by the kernel marker, beside the
//! functions that lift, and the module reads back as the document.

use lash_kernel_dialect::{Environment, Mode, Namespaces};
use lash_kernel_doc as k;

use super::machine;

const MODULE: &str = "async def greet(name: str):
    return await host.echo(name)


async def poke():
    return await host.echo(\"poke\")
";

#[test]
fn a_function_that_cannot_lift_prints_in_kernel_form_and_the_module_reads_back() {
    let effects = machine::effects();
    let namespaces =
        Namespaces::of_effects(&effects, &k::ContractId::new("1"), machine::controls())
            .expect("operations");
    let environment = Environment::new(machine::library(), &namespaces);
    let lowered = crate::lower(MODULE, &environment, Mode::Module).expect("the module lowers");
    let mut document = lowered.document.document().clone();
    // What a canvas insert writes: a perform whose reply no authored call
    // leaves out.
    document
        .functions
        .get_mut(&k::Name::new("poke"))
        .expect("poke is declared")
        .body
        .insert(
            0,
            k::Stmt::Do {
                value: k::Expr::Act(Box::new(k::ActExpr {
                    kind: k::ActKind::Perform {
                        effect: k::EffectName::new("host.echo").expect("an operation"),
                        result: k::Type::Any,
                        then_yield: false,
                    },
                    args: vec![k::Expr::Literal(k::Literal::Text("inserted".into()))],
                })),
            },
        );
    let printed = crate::print(&document, None, machine::library(), Default::default())
        .expect("the document prints");
    let source = &printed.source;
    let poke = source.find("def poke").expect("poke is printed");
    let main = source.find("def main").expect("main is printed");
    let (greet, poke) = (&source[..poke], &source[poke..main]);
    assert!(
        greet.contains("return await host.echo(name)") && !greet.contains("use kernel"),
        "greet lifts:\n{source}"
    );
    assert!(
        poke.contains("\"use kernel\""),
        "poke is in kernel form:\n{source}"
    );
    assert!(
        source[main..].contains("\"use kernel\""),
        "main is in kernel form:\n{source}"
    );
    let (read, _) = crate::read(source, &document, machine::library(), Default::default())
        .expect("the print reads back");
    assert_eq!(
        read.canonical_bytes(),
        document.canonical_bytes(),
        "{source}"
    );
}
