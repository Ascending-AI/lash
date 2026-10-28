# Dialect conventions

This package declares source iteration and operation-boundary recipes without
linking a parser. Frontends and document authoring use the same declarations.
The generic construction and recognition types live in `lash-kernel-dialect`.

Pass the declarations to `Draft::apply`. The document's export conventions take
priority over its helper evidence when selecting a source convention. Kernel
editing still owns purity, binding, graph and transaction checks. A source
convention also declares its inverse verifier. Parser-free declarations use
`Inverse::Unavailable`; dialect packages install `Inverse::Checked`. Admission
checks the inverse before publishing and keeps the ordinary-lowered document,
with correspondence remapped through the printer's existing equivalence check.
