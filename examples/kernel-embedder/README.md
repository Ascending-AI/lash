The embedder is a library outside lash, depending on kernel crates alone.
Call `embed::<YourMachine>(FAN_OUT, registry, answers)` with two `echo` rows
for arguments 1 and 2. It parses the document, runs both children to their
effects, exports the parked state, discards the machine, imports into a new
machine, and delivers the second child's answer first. Printed output is
therefore `[2, 1]`; the `join all` result is `[1, 2]`.

`lash-kernel-conformance` builds this package as a development dependency.
Its `tests::embedder_fan_out_parks_and_resumes` law runs the example against the concrete machine
and checks the outputs, requests, park and resume. No host process, dialect,
store or service is involved.
