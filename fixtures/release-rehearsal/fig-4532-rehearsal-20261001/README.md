# FIG-4532 rehearsal replay journals

This directory retains the `replay-corpus` leg of the six-leg rehearsal
captured by FIG-4532 under `lash.release-fixtures-manifest.v1`.
Its capture tag is `fig-4532-rehearsal-20261001`, its source commit is
`524ec19fbb08a5f86d506b4173b189285c8eb9ff`, and the capture was not a dry run.
The full rehearsal had 77 files across six nonempty legs. This export keeps
only the three replay journals for the non-required FIG-4097 preparation job.

FIG-4533 adds capture epoch 1 and the recorded step order in place. The effect
records and process-command facts are identical to the captured journals;
the recorded source commit stays unchanged. Epoch 1 is the default
`JOURNAL_LOGIC_EPOCH` at that source commit. The full tagged release corpus,
its verifier and its read-back gate remain owned by FIG-4495 at the cut.

FIG-4805 adds the eleven `service-<Service>` handler journals beside them,
recorded at `6b665cd0a405d3abe245d6473eb5085ab2d79add` from the real handlers
on the server double. They are not part of the FIG-4532 capture; the cut
regenerates the whole corpus.

FIG-4852 regenerates the ten service journals from the current handlers and
removes the accounting service. The three original FIG-4532 controller
journals remain the captured historical evidence; the service journals are
the current preparation corpus, generated with the replay-corpus writer.
