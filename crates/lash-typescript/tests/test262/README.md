# Curated test262 slice

These executable fixtures are adapted from test262 commit
`3655e7464de3d52643ecddd4b5f9f4f3e7f62398` and retain the upstream BSD license.
Each fixture names its source test262 path. Adaptation replaces the test262 harness
(`assert.sameValue`, `$ERROR`, and `Test262Error`) with one `finish(boolean)` result;
the semantic expression under test is unchanged. The Rust runner sends every file
through the real TypeScript parse -> normalized AST -> shared AST -> heap VM path.

Selection rule: take at least one positive case for every accepted semantic class
that does not depend on an explicitly rejected feature. Prefer primitive-only tests
whose assertion can be represented exactly by `finish(boolean)`. The slice covers
expressions/statements, coercion, strict and loose equality, template literals,
closures, exceptions, ternary/logical selection, Number edge display, and accepted
String methods. The native integration suite covers aliasing, host JSON boundaries,
diagnostics, signatures, and durability, which are Lash-specific rather than test262.

Do not add a test262 case by weakening the dialect. If its dependencies are outside
the accepted set, add a named rejection test or defer the case until that feature is
implemented exactly.
