# 0143: Plugins are fully trusted code

## Status

Accepted.

## Decision

Plugins run as trusted, in-process Rust with the host's full authority. Lash
provides no sandbox, capability confinement, or CPU or memory isolation for
plugin code. A plugin can use everything the host process can use.

Timers and `catch_unwind` help liveness and attribution. They do not contain
malicious code, prevent process termination, or stop a synchronous callback
from blocking its executor. Host limits bound admitted runtime data and the
runtime's work; they do not isolate arbitrary Rust run by a plugin.

An isolated guest boundary is out of scope until a host needs untrusted
plugins. The typed registration entries and executable binders introduced in
FIG-6087 are the seam such a boundary would use. It would need its own trust,
resource and communication contracts; the current plugin API promises none.

## Consequences

Installing a plugin grants host authority. Plugin certification verifies the
runtime's recording, publication, claim and attribution contracts on the
scenarios actually exercised. It does not establish containment or safety of
plugin code. An unavailable scenario remains unavailable evidence.
