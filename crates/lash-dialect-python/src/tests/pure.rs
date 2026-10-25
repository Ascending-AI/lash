//! Pure Python values and operations use the kernel value model.

use super::machine::run;

fn observes(source: &str, expected: &[&str]) {
    let ran = run(source, &[]);
    assert_eq!(ran.end, "ok", "{source}\n{:?}", ran.lines());
    assert_eq!(ran.lines(), expected, "{source}");
}

#[test]
fn python_radix_and_bitwise_results_are_exact_and_bounded() {
    observes(
        r#"a = 0xffffffffffffffffffffffff
print(a, (a << 5) >> 5, ~a, a & 255, a | 256, a ^ -1)
print(-9 >> 2, -3 & 6, -3 | 6, -3 ^ 6)
try:
    1 << -1
except ValueError:
    print('negative shift')
print(0 >> 99999999999999999999999999)
"#,
        &[
            "79228162514264337593543950335 79228162514264337593543950335 -79228162514264337593543950336 255 79228162514264337593543950335 -79228162514264337593543950336",
            "-3 4 -1 -5",
            "negative shift",
            "0",
        ],
    );
}

#[test]
fn bytes_preserve_octets_iteration_and_decode() {
    observes(
        r#"a = b'a\x00\xff'
print(a, len(a), a[-1], list(a), a[::-1])
print(bytes([65, 66]) + b'C', bytes(3), bytes('é', 'utf-8').decode())
print(b'ab' * 2, 97 in b'ab', b'a' in b'ab', bool(b''))
"#,
        &[
            r#"b'a\x00\xff' 3 255 [97, 0, 255] b'\xff\x00a'"#,
            r#"b'ABC' b'\x00\x00\x00' é"#,
            "b'abab' True True False",
        ],
    );
}

#[test]
fn ellipsis_is_a_guest_singleton_distinct_from_none() {
    observes(
        "a = ...\nprint(a, repr(a), a is ..., a is None, type(a).__name__)\n",
        &["Ellipsis Ellipsis True False ellipsis"],
    );
}

#[test]
fn computed_format_specs_run_in_python_order() {
    observes(
        r#"log = []
def tick(value):
    log.append(value)
    return value
print(f'{tick(12):{tick(6)}.{tick(2)}f}', log)
x = 'é'
print(f'{1.0:.3}|{100.0:.3}|{0.0:.1}|{1.0:#.3g}|{0:zf}')
print(f'{x=}|{x!a}|{12345:010,}|{-0.0:z.1f}|{12345:.3g}|{65:c}')
"#,
        &[
            " 12.00 [12, 6, 2]",
            "1.0|1e+02|0e+00|1.00|0.000000",
            r#"x='é'|'\xe9'|00,012,345|0.0|1.23e+04|A"#,
        ],
    );
}

#[test]
fn percent_formatting_preserves_mapping_width_precision_and_errors() {
    observes(
        r#"fmt = '%(name)s:%(n)04d'
print(fmt % {'name': 'item', 'n': 7})
print('%*.*f|%#x|%r|%%' % (7, 2, 1.25, 255, 'é'))
try:
    '%d' % 'bad'
except TypeError:
    print('type')
"#,
        &["item:0007", "   1.25|0xff|'é'|%", "type"],
    );
}

#[test]
fn pure_attributes_preserve_bounds_and_missing_behavior() {
    observes(
        r#"print((7).real, (7).imag, (7).numerator, (7).denominator)
print((1.5).real, (1.5).imag, (1.5).is_integer(), (1.5).as_integer_ratio())
print((-0.0).hex(), (0.1).hex(), (7).bit_length(), (-7).bit_count(), (1.5).conjugate())
f = 'AbC'.lower
print(f(), getattr('abc', 'upper')(), getattr(7, 'missing', 42), hasattr(7, 'real'), hasattr(7, '__class__'))
"#,
        &[
            "7 0 7 1",
            "1.5 0.0 False (3, 2)",
            "-0x0.0p+0 0x1.999999999999ap-4 3 3 1.5",
            "abc ABC 42 True False",
        ],
    );
}

#[test]
fn type_attributes_expose_unbound_partition_methods() {
    observes(
        r#"partition = str.partition
print(partition('a:b:c', ':'))
print(getattr(str, 'rpartition')('a:b:c', ':'))
print('partition' in dir(str), 'partition' in dir(bytes))
partition = bytes.partition
print(partition(b'\x00\xff:\xff', b'\xff'))
print(getattr(b'a:b:c', 'rpartition')(b':'))
for value, separator in [(b'abc', b''), (b'abc', ':'), ('abc', b':')]:
    try:
        value.partition(separator)
    except ValueError:
        print('empty separator')
    except TypeError:
        print('separator type')
"#,
        &[
            "('a', ':', 'b:c')",
            "('a:b', ':', 'c')",
            "True True",
            r#"(b'\x00', b'\xff', b':\xff')"#,
            "(b'a:b', b':', b'c')",
            "empty separator",
            "separator type",
            "separator type",
        ],
    );
}

#[test]
fn string_methods_preserve_python_bounds_and_missing_behavior() {
    observes(
        r#"print('hello WORLD'.title(), 'Straße'.casefold(), 'aBé'.swapcase())
print('a:b:c'.partition(':'), 'a:b:c'.rpartition(':'), 'abc'.removeprefix('a'), 'abc'.removesuffix('c'))
print('a'.center(5, '-'), 'a'.ljust(3, '.'), 'a'.rjust(3, '.'), '-7'.zfill(4), 'a\tb'.expandtabs(4))
print('abcz'.translate('x'.maketrans('ab', 'xy', 'z')), '{name}'.format_map({'name':'ok'}), 'ΟΣ'.title())
print('é'.isalpha(), '123'.isdecimal(), '²'.isdigit(), 'a2'.isalnum(), 'abc'.islower(), 'ABC'.isupper(), 'A B'.istitle(), ' '.isspace(), ''.isprintable(), 'abc_2'.isidentifier())
"#,
        &[
            "Hello World strasse AbÉ",
            "('a', ':', 'b:c') ('a:b', ':', 'c') bc ab",
            "--a-- a.. ..a -007 a   b",
            "xyc ok Ος",
            "True True True True True True True True True True",
        ],
    );
}

#[test]
fn optional_method_arguments_preserve_python_bounds() {
    observes(
        r#"print(' a b c '.split(None, 1), 'a:b:c'.split(':', 1), 'xyabcxy'.strip('xy'))
print('aaaa'.replace('a', 'b', 2), 'abcabc'.find('bc', 2, 6), 'abcabc'.index('bc', 2), [1,2,1].index(1, 1))
print('abcdef'.startswith(('bc','ab'), 0, 3), 'abcdef'.endswith(('de','ef'), 2))
"#,
        &["['a', 'b c '] ['a', 'b:c'] abc", "bbaa 4 4 2", "True True"],
    );
}

#[test]
fn set_methods_preserve_python_membership_and_aliases() {
    observes(
        r#"s = {1,2}
print(sorted(s.symmetric_difference({2,3})), s.issubset({1,2,3}), s.issuperset({1}), s.isdisjoint({3}))
s.symmetric_difference_update({2,3})
print(sorted(s))
"#,
        &["[1, 3] True True True", "[1, 3]"],
    );
}

#[test]
fn optional_builtin_arguments_preserve_python_order_and_defaults() {
    observes(
        r#"print(dict([('a', 1)], b=2))
print(min([], default=9), max(['aa', 'b'], key=lambda x: len(x)), sorted(['aa', 'b'], key=lambda x: len(x), reverse=True))
print(list(zip([1,2], [3,4], strict=True)))
try:
    list(zip([1], [], strict=True))
except ValueError:
    print('strict')
print('x', 'y', sep=':', end='!', flush=True)
"#,
        &[
            "{'a': 1, 'b': 2}",
            "9 aa ['aa', 'b']",
            "[(1, 3), (2, 4)]",
            "strict",
            "x:y!",
        ],
    );
}

#[test]
fn math_operations_have_host_independent_results() {
    observes(
        r#"import math as m
from math import sqrt
print(sqrt(9), m.floor(-1.5), m.ceil(-1.5), m.gcd(84, 30), m.factorial(6))
print(m.isfinite(m.pi), m.isnan(m.nan), m.isinf(m.inf), m.sin(0), m.log(8, 2))
try:
    m.sqrt(-1)
except ValueError:
    print('domain')
for function in [m.log2, m.gamma]:
    try:
        function(0)
    except ValueError:
        print('pole')
"#,
        &[
            "3.0 -2 -1 6 720",
            "True True True 0.0 3.0",
            "domain",
            "pole",
            "pole",
        ],
    );
}

#[test]
fn json_operations_have_host_independent_results() {
    observes(
        r#"import json
loads = json.loads
print(loads('{"n":123456789012345678901234567890,"a":[true,null]}'))
print(json.dumps({'é': [1, True, None]}, ensure_ascii=True))
print(json.dumps({'b': 2, 'a': 1}, sort_keys=True, separators=(',', ':')))
"#,
        &[
            "{'n': 123456789012345678901234567890, 'a': [True, None]}",
            r#"{"\u00e9": [1, true, null]}"#,
            r#"{"a":1,"b":2}"#,
        ],
    );
}

#[test]
fn string_format_preserves_nested_specs_and_missing_behavior() {
    observes(
        "print('{name}:{0:0{width}d}:{1[answer]!r}'.format(7, {'answer': 'é'}, name='item', width=4))\nprint('{{{}}}'.format(3))\ntry:\n    '{missing}'.format()\nexcept KeyError:\n    print('missing')\n",
        &["item:0007:'é'", "{3}", "missing"],
    );
}

#[test]
fn dict_methods_preserve_keywords_order_and_aliases() {
    observes(
        "d = {'a': 1}\nd.update([('b', 2)], a=3, c=4)\nprint(d, d.popitem(), d, {}.fromkeys(['x', 'y'], 7))\ns = {1}\ns.update([2], [3])\nprint(sorted(s.union([4], [5])), sorted(s.intersection([1,2], [2,3])), sorted(s.difference([1], [3])))\n",
        &[
            "{'a': 3, 'b': 2} ('c', 4) {'a': 3, 'b': 2} {'x': 7, 'y': 7}",
            "[1, 2, 3, 4, 5] [2] [2]",
        ],
    );
}

#[test]
fn declared_package_attributes_preserve_the_typed_surface_and_defaults() {
    use lash_kernel_dialect::{
        CellEnvironment, Environment, FunctionMember, Mode, NamespaceKind, NamespaceMember,
        Namespaces,
    };
    use lash_vocabulary::package::Visibility;
    use lash_vocabulary::{Convention, Name, Param, QualifiedName, Signature, Type};
    use std::collections::{BTreeMap, BTreeSet};
    let name = QualifiedName {
        package: lash_vocabulary::PackagePath::parse("sample").unwrap(),
        member: lash_vocabulary::Segment::new("echo").unwrap(),
        type_member: None,
    };
    let signature = Signature {
        params: vec![Param {
            name: Name::new("value"),
            ty: Type::Int,
            optional: false,
        }],
        result: Type::Int,
    };
    let namespaces = Namespaces::new(
        BTreeMap::from([(
            name.package.clone(),
            BTreeMap::from([(
                name.member.clone(),
                NamespaceMember {
                    visibility: Visibility::Public,
                    kind: NamespaceKind::Function(FunctionMember {
                        name: name.clone(),
                        convention: Convention::Direct,
                        signature: signature.clone(),
                        asynchronous: false,
                        rest: false,
                    }),
                },
            )]),
        )]),
        BTreeSet::new(),
        &|_| false,
    )
    .unwrap();
    let environment = Environment::new(super::machine::library(), &namespaces);
    let lowered = crate::lower("f = getattr(sample, 'echo')\nchosen = 'echo'\nprint(hasattr(sample, chosen), hasattr(sample, 'missing'), getattr(sample, 'missing', 7))\n", &environment, Mode::Cell(CellEnvironment::first())).unwrap();
    let members: Vec<_> = lowered
        .document
        .imports
        .values()
        .filter(|import| import.name == name)
        .collect();
    assert_eq!(members.len(), 1);
    assert!(
        matches!(&members[0].kind, lash_vocabulary::ImportKind::Code { signature: found, convention: Convention::Direct } if found == &signature)
    );
}

/// FIG-7325: integer byte conversions retain defaults, signed boundaries,
/// both orders, exact large integers and argument binding through aliases.
#[test]
fn integer_bytes_match_python_defaults_signed_ranges_and_aliases() {
    observes(
        r#"print((65).to_bytes(), int.from_bytes(b'A'), int.from_bytes([1, 2], 'little'))
for order in ['big', 'little']:
    large = (1 << 100) + 7
    encoded = large.to_bytes(16, order)
    print(int.from_bytes(encoded, order) == large)
    encoded = (-129).to_bytes(2, order, signed=True)
    print(list(encoded), int.from_bytes(encoded, order, signed=True))
print((0).to_bytes(0), (-1).to_bytes(0, signed=True), int.from_bytes(b'', signed=True))
encode = (128).to_bytes
decode = int.from_bytes
for function, args, keywords in [
    (encode, (1,), {'signed': True}),
    ((-129).to_bytes, (1,), {'signed': True}),
    ((-1).to_bytes, (), {}),
    (encode, (-1,), {}),
    (encode, (-1, 'bad'), {}),
    (encode, (1, None), {}),
    (encode, (1, 'big', True), {}),
    (decode, (b'A', 'big', True), {}),
    (decode, (b'A',), {'unknown': 1}),
    (decode, (3,), {}),
    (decode, ([256],), {}),
]:
    try:
        function(*args, **keywords)
    except (TypeError, ValueError, OverflowError) as error:
        print(type(error).__name__)
print(decode(bytes=[255], signed=True), (0).from_bytes([128], signed=True))
print(True.to_bytes(), 'from_bytes' in dir(int))
seen = []
def next_octet():
    seen.append(1)
    return 256
try:
    decode(iter(next_octet, None))
except ValueError:
    print(seen)
"#,
        &[
            "b'A' 65 513",
            "True",
            "[255, 127] -129",
            "True",
            "[127, 255] -129",
            "b'' b'' 0",
            "OverflowError",
            "OverflowError",
            "OverflowError",
            "ValueError",
            "ValueError",
            "TypeError",
            "TypeError",
            "TypeError",
            "TypeError",
            "TypeError",
            "ValueError",
            "-1 -128",
            "b'\\x01' True",
            "[1]",
        ],
    );
}
