#!/usr/bin/env python3
"""Require version bumps when versioned wire or persistence shapes change.

The guarded surfaces live in ``scripts/versioned-surfaces.toml``.  Each guard
compares a deliberately narrow projection of the merge-base tree with the head
tree; a changed projection requires the head's version constant to be strictly
greater than the merge-base value.

For pull requests, this guarantee assumes CI checks a current GitHub merge ref
whose target-branch parent is the latest protected-branch tip. Repositories must
enforce an up-to-date branch or merge queue; a stale merge ref can produce an
older merge-base and cannot prove that independently-landed bumps stay ordered.

Registering a surface -- enrolling a shape that may move in the same change,
with no base version to be strictly greater than -- is a burned one-time
baseline in ``REGISTRATION_BASELINES``, never a category the check infers.  An
inferred category would read a renamed or relocated constant as brand new and
silently un-guard a live surface for one change; a pinned key and fingerprint
cannot be reached by any refactor.

Renaming identifiers across a guarded surface without moving the format it
versions is the other burned one-time baseline, ``IDENTIFIER_RENAME_BASELINES``,
pinned the same way: the guards project guarded text, so a retyped variant reads
as a shape change even when the serialized bytes are identical, and only a
reviewer can say which it was.

Guard hashing ignores only allowlisted non-wire derives inside top-level
``derive`` attributes and ``derive`` entries of ``cfg_attr``. Wire-producing
and unknown derives remain in the preimage, as do namespaced attributes, macro
arguments, and every ``serde`` attribute.

Only the Python standard library is used so the check can run before the Rust
toolchain is installed.  Pull-request CI passes the PR merge-base explicitly.

The gate is paused until the lash 1.0 cut: while ``tools/release-mode.toml``
carries ``pre_release = true`` it reports "paused pre-1.0" and exits 0
(FIG-3660).
"""

from __future__ import annotations

import argparse
from dataclasses import dataclass
import fnmatch
import hashlib
from pathlib import Path
import re
import subprocess
import sys
import tomllib
from typing import Iterable

sys.path.insert(0, str(Path(__file__).resolve().parent))
from release_mode import pre_release  # noqa: E402


ROOT = Path(__file__).resolve().parents[1]
DEFAULT_CONFIG = Path(__file__).with_name("versioned-surfaces.toml")

# Burned one-time proofs that a surface is being registered rather than
# changed. A registration has no merge-base version to be strictly greater
# than, so it is excused from the bump it would otherwise owe -- and that
# excuse is granted only to these exact surface keys carrying these exact
# guarded bytes, never to a category the check infers.
#
# Inference was the earlier design and it was wrong: it asked whether the
# surface key was absent from the merge-base inventory, which is also true of a
# constant someone renamed or moved to another file. An innocent refactor
# therefore looked identical to a registration and un-guarded the surface for
# that change. A pinned key plus a fingerprint of the guarded shape at the
# moment of enrolment answers the real question instead -- "is this the one
# enrolment we reviewed?" -- and no rename can answer it yes.
#
# The fingerprint covers the surface's whole head guard signature: every
# guarded path and symbol, trivia-stripped, in the order the guards declare
# them. A failing check prints the fingerprint it computed, so burning a new
# registration is a deliberate, diffable act by a human who read the shape.
# Entries stay after the surface lands; they are dead-but-honest history, and
# re-adding a removed entry over a live constant is not a registration.
REGISTRATION_BASELINES = {
    # FIG-3672 P9: a code cell journals a gate peek at each cancel checkpoint
    # its VM reaches, placed by lashlang's instruction accounting (compiler
    # emission, builtin charges, yield granularity, the checkpoint schedule).
    # That accounting had no version; it is pinned into cell journal grammar 4.
    "crates/lashlang/src/runtime/mod.rs:INSTRUCTION_ACCOUNTING_VERSION": (
        "sha256:639a18a5da4154cd2ba54a8414faa1bea4e6842a2fd0e84ad1b004347d9f6d08"
    ),
    # FIG-3587: a code cell's journal grammar is a new versioned surface. It
    # was the replay-key grammar alone; it now also covers the ambient binding
    # set a cell journals before its first effect and links against on
    # redrive, which had no version constant.
    "crates/lash-lashlang-runtime/src/replay_run.rs:LASHLANG_CELL_JOURNAL_GRAMMAR_VERSION": (
        "sha256:5edb043201c544b6a95a6b7c6f73538be4a0f751e2d4cbaa3f76ad9f2b741514"
    ),
    # FIG-3586: the issue-ordinal replay-key grammar is a new versioned
    # surface. The call-site keys it replaces had no version constant: they
    # rode the VM ABI and segment-state versions, which this change bumps too.
    "crates/lash-lashlang-runtime/src/replay_run.rs:LASHLANG_REPLAY_KEY_GRAMMAR_VERSION": (
        "sha256:5adc7518b058d7f67ab3df9579aefa05bce175f493bf8b78dd5aabde8979228f"
    ),
    # FIG-3598: the Restate effect-group protocol -- the EffectGroupIndex
    # handlers' wire, the dispatch workflow's wire and the index record Restate
    # retains -- had no version, so an in-flight invocation replaying an older
    # journal met a mismatch Restate retried without end. A new surface, not a
    # rename: the constant arrives with the stamp and the handler-entry refusal.
    "crates/lash-restate/src/effect_group/protocol.rs:EFFECT_GROUP_INDEX_PROTOCOL_VERSION": (
        "sha256:fd6a71a1ea94732fbeeae9956ab0a52d45396ce0d818f5540a6a6fc73eb5900e"
    ),
    # FIG-3464: the durable process-effect outcome is a new runtime-owned event
    # vocabulary, not a rename of an earlier versioned payload.
    "crates/lash-core-execution/src/runtime/process/effect_summary.rs:PROCESS_EVENT_VOCABULARY_VERSION": (
        "sha256:e099c586c0ce0a2d9f284875f03f518c917553448f98c417b6e64734479056c3"
    ),
    # FIG-3544: source-key replay of a pending turn input compares a digest
    # written once at admission. A new durable identity family, not a rename:
    # the digest had no constant before this change. The guard covers the
    # preimage grammar and the TurnInput serde form hashed as one JSON leaf.
    "crates/lash-core-store/src/turn_input_vocabulary.rs:TURN_INPUT_SUBMISSION_FAMILY_VERSION": (
        "sha256:635cac6b7987574524c8b449f38a24fbb17eacffd2ba38777193817c12ce0e0d"
    ),
    # FIG-2266: ADR 0099 sections 6 and 13 mint the semantic settlement a tool
    # child of a durable effect group carries on its outcome. A new durable
    # format, not a rename: the settlement had no constant before this change,
    # and it is deliberately separate from the request's so the two lanes do
    # not force each other's bumps. Re-pinned to the reviewed shape after the
    # hostile-review rounds grew `ToolUsageDelta` (the full usage identity),
    # `ToolSettlement.usage`, and the guard's full serialized closure — the
    # pin now covers every nested payload type review approved.
    "crates/lash-core-execution/src/runtime/effect/tool_settlement.rs:TOOL_SETTLEMENT_VERSION": (
        "sha256:aedb87be6d8a9e0505a37634d2d2b0f0ed54758856935251eec2878e6f4ab66d"
    ),
    # FIG-2266: ADR 0099 section 13 journals the facts one atomic tool attempt
    # produced on the attempt's own outcome, restored on replay. A new durable
    # format riding the shared `ToolAttempt` arm, guarded separately from the
    # settlement because the two carriers move for different reasons. Re-pinned
    # alongside the settlement for the same closure growth.
    "crates/lash-core-execution/src/runtime/effect/tool_settlement.rs:TOOL_ATTEMPT_CAPTURE_VERSION": (
        "sha256:eb503301a493453e2312503176fd7f6a9a24721cb53c398432928c86db28fcec"
    ),
    # FIG-3429: the tool-child rebind checklist enrols REBIND_FIELDS alongside
    # the struct it rules, so a field added to ToolDispatchContext is a
    # guarded-shape change the same day it lands. New surface, not renamed.
    "crates/lash-core-execution/src/tool_dispatch/context.rs:TOOL_CHILD_REBIND_VERSION": (
        "sha256:cba79f85602c06a7d319e64f4f0953f543b8aa7d83205a3eee235085e129391a"
    ),
    # FIG-3420: ADR 0099 section 6 / ADR 0100 mint the journaled presentation
    # record — the model-facing return plus the retained-artifact refs — as a
    # new durable format carried on the `PresentToolResult` outcome. The
    # constant has no merge-base value because the surface is new, not renamed.
    "crates/lash-core-execution/src/runtime/effect/tool_presentation.rs:TOOL_PRESENTATION_VERSION": (
        "sha256:192ec036291f132d064121e24ff37547d980a99236d9f26262c9d7e9890a5286"
    ),
    # FIG-3408: ADR 0099 section 3 mints the retained tool-child request and the
    # invocation-level tool command together, as one new durable format. The
    # constant has no merge-base value because the surface is new, not renamed.
    "crates/lash-core-execution/src/runtime/effect/tool_child.rs:TOOL_CHILD_REQUEST_VERSION": (
        "sha256:296da08a0c7d581e875b1494223ff65c68e01dc40d73614c0f1aa8992390f0ba"
    ),
    # FIG-3672 (P1): the Restate effect journal -- every recorded effect's
    # `lash:{replay_key}` ctx.run entry -- had no version, so there was nowhere
    # to refuse an old history. A new surface, not a rename: the constant
    # arrives with the stamp and the controller's typed refusal (ADR 0105 §12).
    "crates/lash-restate/src/controller/effect_journal.rs:EFFECT_JOURNAL_VERSION": (
        "sha256:8b4c2577687f74745c963232314f93ea521212275a72e2a89a7b46e6ffc5c1ec"
    ),
    # FIG-3600 (S5): the Restate session driver's handlers (LashSession,
    # LashTurn) are new; their requests carry the generation of the commands
    # their journals lead with (ADR 0105 section 12). A new surface, not a
    # rename.
    "crates/lash-restate/src/session_driver.rs:LASH_SESSION_DRIVE_VERSION": (
        "sha256:2542171f2751b0fd3309cb3ce10fb91cea589529d0c0cb98ea226f044a454173"
    ),
    # FIG-3588: the Restate process handler's leading journaled commands (the
    # segment admission verdict and start steps) are a new versioned surface,
    # registered once with the lead's approval (2026-09-24).
    "crates/lash-restate/src/process/admission.rs:RESTATE_PROCESS_JOURNAL_VERSION": (
        "sha256:a7269845c206537cd698e9bedff49e47468580d4dee35695fa8a2f9b5b9c2593"
    ),
    # FIG-1128: the v2 Restate durable-wait request enrolls an explicit
    # absolute-deadline wire after retiring the unversioned relative timeout.
    "crates/lash-restate/src/durable_wait.rs:DURABLE_WAIT_REQUEST_VERSION": (
        "sha256:8688d6e37303f8ae6cd69afdc4cdb6f977811cf4c60429b1a95774efa635c8fb"
    ),
    # FIG-2945: the public serialized sans-IO turn checkpoint is enrolled at
    # v2 when completed pre-dispatch reporting joins its pending effect vocabulary.
    "crates/lash-sansio/src/sansio/machine_state.rs:TURN_CHECKPOINT_SCHEMA_VERSION": (
        "sha256:ee760686edf120042dd150c32569bc1a4d46d56ab14a165c140351ec6db0a6c5"
    ),
    # FIG-2164: native channel surfaces are new relative to main. Parked
    # state honestly advanced from lane v1 to v2 when unused prose was removed;
    # transport v1 stamps the previously unversioned lane envelope.
    "crates/lash-protocol-rlm/src/native/state.rs:NATIVE_DRIVER_STATE_VERSION": (
        "sha256:6ecfecc263e22e361f115d31643a605c8c935521fbe23f0bd47cc398acf50860"
    ),
    "crates/lash-protocol-rlm/src/native/transport.rs:NATIVE_TRANSPORT_VERSION": (
        "sha256:0d697b5a1acbc0565660971dd40cc17ec0eb14e8190c07381f4dd8353674ad8e"
    ),
    # FIG-1529: enrolment of the durable graph-node body, whose already-current
    # shape gained its schema_version stamp in the same change.
    "crates/lash-core/src/session_graph.rs:SESSION_NODE_BODY_SCHEMA_VERSION": (
        "sha256:bcb22b869fe0b86eb9a507b5f1841b733360cfba1164a407d189648998cf1dc9"
    ),
    # FIG-2480: reviewer-confirmed one-time baseline pinning the v1
    # registration state of the semantic-boundary request-identity encodings.
    # Two independent reviews cleared the encoding; domain discovery and schema
    # congruence fixes left the identity bytes unchanged, so version 1 remains
    # current. The three constants share one guarded shape (the shared request
    # projection hashed under operation-owned domains) and one fingerprint.
    # The baseline pins this STATE; any further guarded-shape drift re-fails
    # the gate.
    "crates/lash-core/src/store/semantic_boundary.rs:RECORD_CONFIG_REQUEST_IDENTITY_ENCODING_VERSION": (
        "sha256:0b06228ca33674b8cbfcb73a02c46a03f6629dcc7949f9da785a43c9082bcfdc"
    ),
    "crates/lash-core/src/store/semantic_boundary.rs:CREATE_SESSION_REQUEST_IDENTITY_ENCODING_VERSION": (
        "sha256:0b06228ca33674b8cbfcb73a02c46a03f6629dcc7949f9da785a43c9082bcfdc"
    ),
    "crates/lash-core/src/store/semantic_boundary.rs:USAGE_LEDGER_REQUEST_IDENTITY_ENCODING_VERSION": (
        "sha256:0b06228ca33674b8cbfcb73a02c46a03f6629dcc7949f9da785a43c9082bcfdc"
    ),

    # FIG-2996 part 2: the durable process-lease block (PROCESS_LEASE_SCHEMA_VERSION,
    # ProcessLease and its version fence) moved verbatim from
    # crates/lash-core/src/runtime/process/model.rs into the sibling
    # model/lease.rs to bring model.rs back under the production file-size
    # budget. Same re-exports, no field, variant, serde attribute or constant
    # changed, so the serialized lease bytes are identical and
    # PROCESS_LEASE_SCHEMA_VERSION stays 2 under its relocated key.
    'crates/lash-core/src/runtime/process/model/lease.rs:PROCESS_LEASE_SCHEMA_VERSION': 'sha256:627d95b22c67fe20640132b6a8fff0b188aa1b107891feaf350487de0194e8f6',
    # FIG-3042 step 4: the durable domain layer was carved verbatim out of
    # lash-core into the new lash-internal-core-store crate. Every surface
    # below kept its file name and its contents; only the crate directory in
    # front of the path changed (crates/lash-core/src/... ->
    # crates/lash-core-store/src/...), and versioned-surfaces.toml follows each
    # file. A surface key is path-qualified, so a relocation reads to the check
    # as a brand-new key with no merge-base value even though the merge-base
    # value is right there under the old path; that is what these entries pin.
    #
    # The reading, written out once for all of them: the move is a `cp` of the
    # file plus a crate boundary. No struct, field, variant, enum arm, derive
    # input, serde attribute, preimage byte expression, tag or constant value
    # changed on any of these surfaces. The visibility widenings the carve
    # required (pub(crate) -> pub on items lash-core still calls) are Rust
    # visibility alone and are invisible to serde, to IdentityEncoder and to
    # every emitted byte. So each constant keeps the value it had at the
    # merge-base: no durable format moved and a bump would publish a false
    # incompatibility to stored data and peers.
    #
    # The corresponding old-path entries above and in
    # IDENTIFIER_RENAME_BASELINES stay as dead-but-honest history. As with
    # every entry here, these pin a STATE and not a transition: any further
    # guarded-shape drift on these surfaces re-fails the gate.
    "crates/lash-core-store/src/store/commit_identity.rs:APPEND_REQUEST_IDENTITY_ENCODING_VERSION": (
        "sha256:d7cc86c3a9018bc4f56119dd666c6f6280ca4b4cc9d881e798d16b1a2ac85042"
    ),
    "crates/lash-core-store/src/store/semantic_boundary.rs:RECORD_CONFIG_REQUEST_IDENTITY_ENCODING_VERSION": (
        "sha256:59fa45d404be44b05157eab4a48ed266b16c3be6a16cf5b7c45dd3410abaa032"
    ),
    "crates/lash-core-store/src/store/semantic_boundary.rs:CREATE_SESSION_REQUEST_IDENTITY_ENCODING_VERSION": (
        "sha256:59fa45d404be44b05157eab4a48ed266b16c3be6a16cf5b7c45dd3410abaa032"
    ),
    "crates/lash-core-store/src/store/semantic_boundary.rs:USAGE_LEDGER_REQUEST_IDENTITY_ENCODING_VERSION": (
        "sha256:59fa45d404be44b05157eab4a48ed266b16c3be6a16cf5b7c45dd3410abaa032"
    ),
    # The preimage helper was already extracted to an effect-identity module by
    # FIG-2828; step 4 moves that module into the store crate and, with the
    # envelope's other half staying behind, the constant travels with the
    # preimage it stamps. constant_path follows it to effect_identity.rs.
    "crates/lash-core-store/src/effect_identity.rs:PROCESS_TRANSFER_FAMILY_VERSION": (
        "sha256:fd1c8180f7527a8a3351c9928c75f65d158f028628c963d741cf07ecf59698d8"
    ),
    "crates/lash-core-store/src/store/runtime_commit.rs:USAGE_PAYLOAD_FAMILY_VERSION": (
        "sha256:e011166157bb61ec231962ddc2f6e0a4643c8a5d0b4169937c0cde690adf39e8"
    ),
    "crates/lash-core-store/src/store/checkpoint.rs:SESSION_CHECKPOINT_SCHEMA_VERSION": (
        "sha256:0df187b51bed2dc8e75316cfc03f011e5bd7aebec9d37816b41033d4eb5101f2"
    ),
    "crates/lash-core-store/src/store/checkpoint.rs:CHECKPOINT_COMPONENT_ENCODING_VERSION": (
        "sha256:7c12433057d269a90e6faaee3529d39e59335b398cc1f655b083a6c9ad632931"
    ),
    "crates/lash-core-store/src/store/mod.rs:SESSION_HEAD_META_SCHEMA_VERSION": (
        "sha256:fec9a971eedc5816bbe7fa4e550838a1ef104fcee6a339d0cf9cf1c2a801cc8a"
    ),
    "crates/lash-core-store/src/session_graph.rs:SESSION_NODE_BODY_SCHEMA_VERSION": (
        "sha256:942d93486d04e890ae21c77550653cbb6a699a3fb036462db826059299f841ec"
    ),
    # PROTOCOL_TURN_OPTIONS_SCHEMA_VERSION was declared in lash-core's lib.rs
    # beside the hand-written codecs that stamp it; the whole envelope -- the
    # struct, both codecs, the wire carrier and the constant -- moved together
    # into its own file in the store crate, so the guard and constant_path both
    # follow to protocol_turn_options.rs.
    "crates/lash-core-store/src/protocol_turn_options.rs:PROTOCOL_TURN_OPTIONS_SCHEMA_VERSION": (
        "sha256:cb04566004239a990b38cecec781c27577b1d2afec0e8e6a6a024aa961e33a85"
    ),
    "crates/lash-core-store/src/store/state_version.rs:CURRENT_SESSION_STATE_VERSION": (
        "sha256:5fb0524a0d534905c775abf9c0051cc48a4e5d844f61fa1be003e3fd3901549c"
    ),
    # FIG-3331: the execution kernel was carved out of lash-core into the new
    # lash-internal-core-execution crate (crates/lash-core/src/... ->
    # crates/lash-core-execution/src/...) and the promise-semantics module into
    # lash-internal-core-effect; versioned-surfaces.toml follows each file, so
    # every key below reads to the check as brand new even though its
    # merge-base value sits under the old path. Same reading as FIG-3042 step
    # 4, taken per file against the merge-base copy: validation.rs, triggers.rs,
    # wake.rs, model/lease.rs and lease_serde.rs are byte-identical; router.rs,
    # causal.rs, tool_execution.rs and promise_semantics.rs differ only in
    # `pub(crate)`/`pub(super)` -> `pub` on items lash-core still calls; and
    # events.rs already re-exported PROCESS_WAKE_DELIVERY_FORMAT_VERSION (3)
    # from the store crate at the merge-base. No struct, field, variant, derive
    # input, serde attribute, preimage byte expression, tag or constant value
    # changed, so each constant keeps its merge-base value. The old-path
    # entries above and in IDENTIFIER_RENAME_BASELINES stay as dead-but-honest
    # history; these pin a STATE, and any further guarded-shape drift on these
    # surfaces re-fails the gate.
    "crates/lash-core-execution/src/runtime/process/validation.rs:PROCESS_REGISTRATION_FAMILY_VERSION": (
        "sha256:e036dc51092b29632b22fd1d8b074653f26b44bf4267135ceb22bc1708997f5c"
    ),
    "crates/lash-core-execution/src/triggers.rs:TRIGGER_COMMAND_FAMILY_VERSION": (
        "sha256:d706bc427b47f782da904ac5d899fceb6418646ce591955b14813c2fb565e480"
    ),
    "crates/lash-core-execution/src/triggers.rs:TRIGGER_OPERATION_ADDRESS_FAMILY_VERSION": (
        "sha256:8a0f11f6e192b8e6fb85b3999babf817810a99777a0f3056c2f0cf27caf913fe"
    ),
    "crates/lash-core-execution/src/triggers/router.rs:TRIGGER_DEFINITION_FAMILY_VERSION": (
        "sha256:e9570d29cdb119af5ec68a51f1f97d39abe1b4712e7f86996e4be2e5197c7d25"
    ),
    "crates/lash-core-execution/src/triggers/router.rs:TRIGGER_LOOKUP_FAMILY_VERSION": (
        "sha256:fdfd5e0edfe519224ee9f9a9b72a6672b8266b4e027395f7edf773c35622cfa7"
    ),
    "crates/lash-core-execution/src/triggers/router.rs:TRIGGER_SOURCE_FAMILY_VERSION": (
        "sha256:52f81238ef0a160fb8aeb58fa497d58e4985405ae5f611dd661915f54d2504d8"
    ),
    "crates/lash-core-execution/src/triggers/router.rs:TRIGGER_DELIVERY_PROCESS_FAMILY_VERSION": (
        "sha256:7116dbf4722d84fd3e9dd350bcfb5413284b81746782ee5203413e19588fe16c"
    ),
    "crates/lash-core-execution/src/triggers/router.rs:DERIVED_TRIGGER_SUBSCRIPTION_FAMILY_VERSION": (
        "sha256:f5bdd39a3ada472b6c4f4fde81ddb10e2b60a0f213c8244f70391c7af82c8da2"
    ),
    "crates/lash-core-execution/src/runtime/causal.rs:DIRECT_EFFECT_FAMILY_VERSION": (
        "sha256:6f221155d7f3a3156c1ad45bc1c4518739177c49569967019ccbf6c5295cbd40"
    ),
    "crates/lash-core-effect/src/promise_semantics.rs:AWAIT_EVENT_FAMILY_VERSION": (
        "sha256:7eb7f84a9dd6b5ca219959f9c3efc43fc236c0f95591a84a69e64eb07c501cd8"
    ),
    "crates/lash-core-execution/src/runtime/process/events.rs:PROCESS_CANCELLATION_FAMILY_VERSION": (
        "sha256:d287175f35bf44bfdb0efa7259fc2bec3487c75552cf604c12139b467dd1e098"
    ),
    "crates/lash-core-execution/src/runtime/process/wake.rs:PROCESS_WAKE_FAMILY_VERSION": (
        "sha256:ee19d6ca4f27c6e57f897ad821e09e78269d1ae1521874d5a06a6405828139af"
    ),
    "crates/lash-core-execution/src/session/tool_execution.rs:TOOL_BATCH_FAMILY_VERSION": (
        "sha256:509ac3693782c0b9bbc41dc2a2f44976c9ec0532ba508d6c23a42809244debe1"
    ),
    "crates/lash-core-execution/src/runtime/process/model/lease.rs:PROCESS_LEASE_SCHEMA_VERSION": (
        "sha256:5ba0ecd22f3c782cd1b68121ca8772b6a2833ed48a54440dc3a614604ebb2dd0"
    ),
    # FIG-3537: the durable runtime-commit receipt (RuntimeCommitReceipt,
    # persisted in lash_runtime_turn_commits.result_json) gains its explicit
    # schema_version stamp, joining the durable-format registry and the
    # payload-shape gate in the same change. The constant has no merge-base
    # value because the surface is new: the receipt was previously unversioned
    # and deliberately excluded from the gate. Registration v1.
    "crates/lash-core-store/src/store/runtime_commit.rs:RUNTIME_COMMIT_RECEIPT_SCHEMA_VERSION": (
        "sha256:04a9c1a7dcf3c3fbd5eed9a3586f1f9bf9de87b67855c96b8a5fa07d4ade330c"
    ),
}

# Burned one-time proofs that a change moved Rust identifiers across a guarded
# surface without moving the format the surface versions. The guards project
# the TEXT of the guarded items, identifiers included, so a rename of a type or
# a variant reads as a shape change even when every serde name, wire tag, and
# emitted fingerprint string is byte-identical on both sides -- and a version
# bump for that would publish a false incompatibility to peers and stored data.
#
# The exemption is granted the same way a registration is, and for the same
# reason: only to these exact surface keys carrying these exact guarded bytes,
# pinned to the head signature the reviewer read. There is no inferred
# "identifier-only" category and there must never be one, because the check
# cannot see whether a rename reached the serialized bytes; a human reads that
# and burns the answer here. Entries stay after the change lands as
# dead-but-honest history.
IDENTIFIER_RENAME_BASELINES = {
    # FIG-3672 P7b: the cell binding set's drift judgement moved to
    # `lash_core::tool_dispatch_surface`, shared with the turn's recorded tool
    # surface, and is now guarded there. The journaled binding record and the
    # fields drift is judged on are byte-for-byte what they were.
    "crates/lash-lashlang-runtime/src/replay_run.rs:LASHLANG_CELL_JOURNAL_GRAMMAR_VERSION": (
        "sha256:08718e57d0f21a6fc0f3b2e1d6011b1f44b51373bf27a195d1a60e0ca9e0055f"
    ),
    # FIG-3521: PROTOCOL_TURN_OPTIONS_SCHEMA_VERSION widened from pub(crate) to
    # pub (and gained a doc comment) so the format manifest re-exports the one
    # definition instead of copying the integer. Its type (u32) and value (1)
    # are unchanged, so `serialize_u32` emits the same stamp and the envelope
    # bytes are identical; the version stays 1.
    'crates/lash-core-store/src/protocol_turn_options.rs:PROTOCOL_TURN_OPTIONS_SCHEMA_VERSION': 'sha256:591c95fb0981dd1af20f7a922eea6961ef3dd81943814720db5204a920f894be',

    # FIG-3469: `AstString` replaced the `CompactString` alias with a local
    # transparent wrapper. Its serde and schema agreement test pins the same
    # JSON string bytes, and the process wire DTO agreement tests pin the
    # custom process-type encoding. The graph, facet, bytecode, and
    # continuation carriers therefore keep their current versions.
    'crates/lashlang/src/lib.rs:BYTECODE_FORMAT_VERSION': 'sha256:a9f3e2866427a4620ff01c0a1a77a77378083975ad9cbaeca59bef8b28c70029',
    'crates/lashlang/src/workflow_graph.rs:WORKFLOW_GRAPH_SCHEMA_VERSION': 'sha256:ba80461ef5b1dd94ef6ab0fb1c19b4ed5be0c9e551e635199c4e4cc62840d18c',
    'crates/lashlang/src/workflow_graph/facets.rs:WORKFLOW_TYPE_FACET_SCHEMA_VERSION': 'sha256:96b314b45059996ec12f1c6d58a1d5885c1c019e19499b316eefa3e553998a41',

    # FIG-2888: the four duplicated identity-projection helpers (event type,
    # value selector, payload leaf, schema leaf) moved once into
    # runtime/process/identity_projection.rs and were renamed
    # project_{trigger,registration}_* -> project_process_*. The projection
    # bodies are byte-identical on both sides, so every emitted preimage is
    # unchanged; PROCESS_REGISTRATION_FAMILY_VERSION stays 7,
    # TRIGGER_COMMAND_FAMILY_VERSION stays 8,
    # TRIGGER_DEFINITION_FAMILY_VERSION stays 5, and
    # TRIGGER_SOURCE_FAMILY_VERSION stays 1.
    'crates/lash-core-execution/src/runtime/process/validation.rs:PROCESS_REGISTRATION_FAMILY_VERSION': 'sha256:50b03d274e75d04f2efe06774b5160589504f4180a967b45f910e80136a81752',
    'crates/lash-core-execution/src/triggers.rs:TRIGGER_COMMAND_FAMILY_VERSION': 'sha256:84b30afdb8f8c0e9e0641d7d2d8de59b96a9f52ce556b151962e26a0789a6db3',
    'crates/lash-core-execution/src/triggers/router.rs:TRIGGER_DEFINITION_FAMILY_VERSION': 'sha256:801fd8f9f95a618b9fada3e0d840c414a2b72b80034413c649542d75a860f48d',
    'crates/lash-core-execution/src/triggers/router.rs:TRIGGER_SOURCE_FAMILY_VERSION': 'sha256:e66f2481ca9987331d4a66480d87afa5bf5e5f79e5a147c0494dc062e97d97cb',

    # FIG-2887: the append-request projection's fifteen private push_* framers
    # became projections onto IdentityEncoder (frozen unframed family, ADR
    # 0097), and the guard re-pointed at the shared encoder methods. The
    # v1/v2/v4 golden corpora prove every emitted byte is identical, so
    # APPEND_REQUEST_IDENTITY_ENCODING_VERSION stays 4.
    # History of the readings this entry supersedes --
    #   FIG-3239: ResponseTextMeta.phase retyped Option<String> ->
    #     Option<ResponsePhase>; serde and the preimage emit the same two wire
    #     strings ('commentary'/'final_answer') at v4.
    #   FIG-3305: `Part` retyped from a flat struct of kind-tagged
    #     `Option`s into an internally-tagged enum whose variants own only
    #     their fields. The serde wire shape and the identity preimage
    #     leaves are byte-identical for every representable part — the
    #     corpus rows that moved did so only because the fixture packed
    #     invalid kind/field pairings the type no longer holds — so
    #     APPEND_REQUEST_IDENTITY_ENCODING_VERSION stays 4 and
    #     SESSION_NODE_BODY_SCHEMA_VERSION stays 16.
    #   FIG-3401: `SessionGraphData.nodes` retyped Vec<SessionNodeRecord> ->
    #     Vec<Arc<SessionNodeRecord>> so snapshot copy-on-write clones node
    #     pointers instead of whole records. `Arc<T>` serializes as `T`, so
    #     every persisted node body is byte-identical — the serde-shape
    #     regression tests pin that — and SESSION_NODE_BODY_SCHEMA_VERSION
    #     stays 17.
    #   FIG-3411: `LlmCallId` gained `PartialOrd`/`Ord` derives so
    #     `UsageDeltaIdentity` can order a BTreeSet. `LlmCallId` serializes
    #     `#[serde(transparent)]` over `String`, so every emitted byte is
    #     identical — SESSION_NODE_BODY_SCHEMA_VERSION stays 18.
    'crates/lash-core-store/src/store/commit_identity.rs:APPEND_REQUEST_IDENTITY_ENCODING_VERSION': 'sha256:fbf343e99da3d0f156255adf90c62cf4e9fc594ed436f40f3ac60796e11ec139',
    'crates/lash-core-store/src/session_graph.rs:SESSION_NODE_BODY_SCHEMA_VERSION': 'sha256:b8e674b0a616e2fc2fce0cd01dc2e488f051a9e047a85d8b9b7f1f90f6a8f2d4',

    # FIG-2784 pass 1 (#1502): `push_causal_ref` in commit_identity.rs gained a
    # two-line doc comment and `#[expect(clippy::expect_used, ...)]` under the
    # workspace-wide expect/unwrap denial. A lint attribute and prose; no
    # field, variant, serde attribute, constant or encoding step changed, so
    # the append-request identity bytes are identical and
    # APPEND_REQUEST_IDENTITY_ENCODING_VERSION stays 4.
    'crates/lash-core/src/store/commit_identity.rs:APPEND_REQUEST_IDENTITY_ENCODING_VERSION': 'sha256:24244e89d86a4ca909abb06815828b9d16c143e416861c3fb3619735dacc7cdf',

    # FIG-2360: the reviewed version fence replaces derived decoding while
    # preserving version-2 serialization and current-format input semantics.
    # Both HARD review scopes confirmed exact JSON/MessagePack output parity.
    'crates/lash-core/src/runtime/process/model.rs:PROCESS_LEASE_SCHEMA_VERSION': 'sha256:b8ae30a35110ff90e730de2de77f97489b8ff82f3482a5e4ddc3fab1568e674e',

    # FIG-2783: restoring the usage_disposition_json explanation comment added
    # two `--` lines inside the SQLite CREATE TABLE body. Comments are inert
    # to SQLite DDL, so the stored catalog is byte-identical and
    # SCHEMA_VERSION stays 70.
    # FIG-3397 PR B supersedes this reading: the tool-batch command is deleted,
    # and with it six RuntimeErrorCode variants no build can raise
    # (runtime_effect_tool_batch_{call_id,call_replay,empty,id},
    # tool_batch_missing_result, tool_batch_result_count_mismatch). Deletion
    # only: every surviving code keeps its wire spelling, and a stored deleted
    # spelling still decodes verbatim as RuntimeErrorCode::ForeignCode and
    # re-encodes to the same bytes. No column or CHECK moved, so the stamp
    # stays (PostgreSQL 116, SQLite 75). Superseded:
    # sha256:b35533b82dd6a0ebe7b53aecc81628a7c59963690956e906edb44165af19aaf1.
    # FIG-3397 PR C supersedes this reading: two RuntimeErrorCode variants are
    # added (aggregate_await_unsettled, effect_group_opener_bound_exceeded).
    # Addition only: every existing code keeps its wire spelling, no stored row
    # can hold a new one before this build writes it, and an older reader
    # decodes a new spelling verbatim as RuntimeErrorCode::ForeignCode and
    # re-encodes it to the same bytes. No column or CHECK moved, so the stamp
    # stays (PostgreSQL 118, SQLite 77). Superseded:
    # sha256:6316b508dd69074999dccc3c4b8ca3f5dd64d254dcd0037d4f56fa44da1a68ee.
    # FIG-3619 supersedes this reading (lead ruling, 2026-09-24): two
    # RuntimeErrorCode variants are added (session_state_version_unsupported,
    # session_state_version_newer_than_runtime), and RuntimeError gains a
    # #[serde(skip)] field carrying the refused generations in-process only.
    # Addition only: every existing code keeps its wire spelling, the stored
    # error shape is unchanged, and an older reader decodes a new spelling
    # verbatim as RuntimeErrorCode::ForeignCode and re-encodes it to the same
    # bytes (a_stored_session_state_refusal_reads_as_a_foreign_code_before_the_codes_existed).
    # No column or CHECK moved, so the stamp stays (SQLite 81). Superseded:
    # sha256:92c2074dae0baa5fa79c72480790752b8e174d1ed3b25a94f4d3114e64f22f74.
    'crates/lash-sqlite-store/src/schema.rs:SCHEMA_VERSION': 'sha256:75c43a7339427ba80ac526ac2164a244ddbf57f422eb7abcb0d40a8f43b34893',

    # FIG-1102: the workbench include! splice became real modules, so every
    # item in state.rs gained pub(crate) and one line was rewrapped. Serde
    # attributes, field and variant names, and constant values are unchanged;
    # an independent reviewer confirmed the serialized bytes are identical, so
    # PRODUCT_EVENT_LOG_FORMAT_VERSION stays 2.
    'examples/agent-workbench/src/main_sections/state.rs:PRODUCT_EVENT_LOG_FORMAT_VERSION': 'sha256:3280d5d3d9cbaf4094f41b0fa908fd5056d44130763f156d67282e9e15f95b03',
    # FIG-1036, one time only: the outcome-suffix vocabulary rename retyped
    # Rust identifiers across these three surfaces while leaving every serde
    # field name, variant name, and emitted fingerprint tag byte-identical, so
    # REMOTE_PROTOCOL_VERSION stays 41, SESSION_NODE_BODY_SCHEMA_VERSION stays
    # 1, and PROCESS_REGISTRATION_FAMILY_VERSION stays 4.
    # Known residual, inherited from REGISTRATION_BASELINES and equally narrow:
    # the baseline pins a STATE, not a transition, so a future change that
    # restores the guarded text to exactly these bytes would re-match and be
    # excused a second time.
    # FIG-1964: the private VersionProbe decode carrier was hoisted from two
    # per-message copies into one shared crate-private helper. The probe is a
    # decode-only view of an existing sibling field; no serialized message
    # carrier, field, tag, or byte changed and REMOTE_PROTOCOL_VERSION 45 is
    # still the wire truth. Ruled on FIG-1964; the baseline pins this state --
    # any further guarded-shape drift in lash-remote-protocol re-fails the gate.
    # FIG-1801: removing the Default derive/impl from remote turn outcomes
    # changed the guarded Rust shape while leaving serde bytes identical;
    # REMOTE_PROTOCOL_VERSION remains 51. Reviewer-confirmed one-time baseline.
    "crates/lash-remote-protocol/src/lib.rs:REMOTE_PROTOCOL_VERSION": (
        "sha256:4f0da66cfa71819ff33a6ca872b6825e3d403c9f4f1c99918ee3de03efea09bc"
    ),
    # Node-body rename baselines, collapsed to one live entry: this is a plain
    # dict, so a second literal for the same key would silently win. History of
    # the readings this entry supersedes --
    #   FIG-2144: ChargeSafetyPolicy is live host configuration omitted from
    #     SessionPolicyWire and RetryDecision.charge_safety is serde-skipped;
    #     the guarded Rust shapes moved, the persisted bytes did not.
    #   FIG-2479: PersistedSessionConfig gained the optional
    #     protocol_turn_options head field -- session-head payload, not a
    #     node-body carrier; the honest bump landed on
    #     SESSION_HEAD_META_SCHEMA_VERSION 5 -> 6.
    #   FIG-2880: removing tool_access's serde default changed head-owned
    #     PersistedSessionConfig only; head version 8 fences those bytes.
    #   FIG-1040: the identity-newtype wave retyped raw `String` id fields to
    #     the transparent NodeId/InputId/BatchId/SessionId/ProcessId/TurnId
    #     newtypes, each #[repr(transparent)] + #[serde(transparent)] with its
    #     JsonSchema delegated to String; the guarded text moved, the bytes
    #     could not. (Superseded state:
    #     sha256:1a59dad63420820eca62f134f5c011b2aa38f4d773e7bc5199cde23ff19a98db.)
    #   All four superseded by FIG-3067.
    # FIG-3067 (live): removing rustdoc from the workspace deletes every
    # #[doc(hidden)] attribute, six of them inside the guarded
    # crates/lash-sansio/src/llm/types.rs region (two ResponseTextMeta fields
    # and four inherent methods). #[doc(hidden)] is read by rustdoc alone: it
    # is not a visibility, not a serde attribute, and not part of any derive
    # input, so the guarded Rust text moved while the serialized bytes cannot
    # have. The whole diff for that file is attribute-line deletions -- no
    # field name, variant name, serde attribute, type or constant value
    # changed -- so SESSION_NODE_BODY_SCHEMA_VERSION stayed 14 at that reading
    # (sha256:3d39e0ec853e4865725d7f3fa5a0a4b0fce67753aa01b98e5ad9c20cb37cc69d).
    # FIG-3042 step 2 (live): the guarded crates/lash-core/src/model.rs file
    # moved verbatim to crates/lash-core-llm/src/model.rs (the path in
    # versioned-surfaces.toml follows it) and two pub(crate) inherent methods
    # with their test (`clamp_generation_options`, `clamped_generation`) left
    # the file for a crate-internal extension trait in lash-core. No struct,
    # field, variant, serde attribute, derive input or constant changed, so the
    # serialized bytes are identical and SESSION_NODE_BODY_SCHEMA_VERSION stays
    # 14. Any further guarded-shape drift re-fails the gate.
    "crates/lash-core/src/session_graph.rs:SESSION_NODE_BODY_SCHEMA_VERSION": (
        "sha256:20844e5b59c7ab9b340af3d7d06b285dbe0740a011ea73e95936311b46ebc11e"
    ),
    "crates/lash-core/src/runtime/process/validation.rs:"
    "PROCESS_REGISTRATION_FAMILY_VERSION": (
        "sha256:fd2a5cd3cc916b265166f95f896524883c047382634e959d152df48e860f9ed7"
    ),
    # FIG-1623 history: a structural envelope hoist kept identical key
    # sets/values while object key order changed (kind first→third); key order
    # was ruled outside the trace wire contract, so no schema bump was owed.
    #
    # FIG-1975: occurrence-scoped node observations. TraceLashlangGraphNode and
    # its observation enum are a host-facing projection REDUCED from trace
    # records by TraceLashlangGraphStore; they are never written into a
    # TraceRecord, so the durable JSONL record schema is byte-identical and no
    # TRACE_SCHEMA_VERSION bump is owed. Ruled on FIG-1975; the baseline pins
    # this state -- any further guarded-shape drift in lash-trace re-fails the
    # gate.
    "crates/lash-trace/src/lib.rs:TRACE_SCHEMA_VERSION": (
        "sha256:235086a4420f7aacaa41b9305ae58b02d325d8b80d1e1024af914b2e8e5c0d3e"
    ),
    # FIG-1792, one time only: the protocol turn options schema version became
    # wire-only. The in-memory field was deleted and a hand-written serializer
    # now stamps the constant, but the emitted struct name, field names, field
    # order, and the stamped value's Rust type (u32) are unchanged, so the
    # bytes are identical in both directions and
    # PROTOCOL_TURN_OPTIONS_SCHEMA_VERSION stays 1. The same change widens the
    # guard to the hand-written codecs, so this fingerprint covers the widened
    # signature; the identical residual applies as above -- the baseline pins a
    # STATE, so restoring exactly these bytes later would re-match.
    # (FIG-1792 superseded state: sha256:2f04d80d453bf0e962e4c3a0eafa7189
    # 7732e60e6922a975388f24160016a0a1.)
    #
    # FIG-2479: ProtocolTurnOptions gained PartialEq/Eq derives so the head
    # config and config patch can carry it in their Eq types. The persisted
    # envelope is emitted by the hand-written serialize_protocol_turn_options
    # codec, which is untouched; serialized bytes are identical and
    # PROTOCOL_TURN_OPTIONS_SCHEMA_VERSION stays 1.
    "crates/lash-core/src/lib.rs:PROTOCOL_TURN_OPTIONS_SCHEMA_VERSION": (
        "sha256:4189e3b8fd5f0589de7882132bf913c74963db9d86bebc531db66f1aface1632"
    ),
    # FIG-1980: tool_execution_grant_json_layout_is_stable witnesses that the
    # serde surface is unchanged, so TOOL_BATCH_FAMILY_VERSION remains 1.
    # FIG-2774: the sole tool_execution.rs change ignores the new presentation
    # field (`inline`, prompt-only: it decides catalogue rendering and never
    # reaches the wire) in an exhaustive manifest destructure. No preimage
    # field, tag, serialization call or byte expression changes; batch
    # identity stays v1.
    'crates/lash-core/src/session/tool_execution.rs:TOOL_BATCH_FAMILY_VERSION': 'sha256:3f7b64de0d53961e1c8e33f8ca5f4284c2aca7a3cf364a29b49f5cb299b5767d',
    # FIG-3460: ToolInvocation gained an issuing workflow-node id used only to
    # link tool trace events back to the language node. The batch preimage's
    # exhaustive destructure explicitly ignores it, and
    # issuing_node_id_does_not_change_tool_batch_identity proves both the raw
    # preimage and rendered v2 identity remain byte-identical.
    # FIG-3397 PR B supersedes that reading: ToolBatchOccurrence is renamed
    # ToolGroupOccurrence (the ordinal is group-key material now), and
    # tool_invocation_batch_preimage folds the same identity tag under the same
    # family string, so every batch id is byte-identical and
    # TOOL_BATCH_FAMILY_VERSION stays 2. Superseded:
    # sha256:d71ea72d3b80d401109b0ca80de601bc31ff61e813392e822257ac1cda7c9644.
    # FIG-3587 supersedes that reading: `ToolInvocation` gained
    # `recorded_binding`, the binding a replayed code cell recorded for a call
    # whose live tool drifted, and the preimage's exhaustive destructure names
    # it as ignored (`recorded_binding: _`), so every v3 batch id is
    # byte-identical and the family stays 3. Superseded:
    # sha256:5726d4993280651cf025e3a365a6b395c212bc4b503fffb0e52dec6e89011f82.
    'crates/lash-core-execution/src/session/tool_execution.rs:TOOL_BATCH_FAMILY_VERSION': 'sha256:a9bbd29219386d6b13570db71e62c00efc02fd1ac909efd6747e622d7a6918dd',
    # FIG-2234 fix 4: the generated schema.sql header comment was aligned to
    # component version 64 (bumped in lib.rs by the BLAKE3 cutover without
    # regenerating the artifact header). Comment-only; the executed DDL is
    # semantically unchanged, so SCHEMA_VERSION stays 64.
    'crates/lash-postgres-store/src/lib.rs:SCHEMA_VERSION': 'sha256:ad84fa487a6f19843092785c6fffa288262eae5af183d394465d4c3e21c93e9c',
    # FIG-2235: the changed-component carrier gained a serde-skipped body_ref
    # reused across commit projections; rmp/JSON serialization is
    # byte-identical and round-trip reconstructs the digest, so
    # CHECKPOINT_COMPONENT_ENCODING_VERSION stays 2 (a bump would refuse
    # every existing v2 checkpoint).
    'crates/lash-core/src/store/checkpoint.rs:CHECKPOINT_COMPONENT_ENCODING_VERSION': 'sha256:ae8d9e989bceae4d42097ec18cb935616de4f30ccdb35507e04fb0fd90438da8',
    # FIG-3402: checkpoint component bodies retyped Vec<u8> -> Arc<[u8]> behind
    # a serialize_bytes shim (arc_serde_bytes), so rmp/JSON bytes are
    # byte-identical and legacy bytes still decode; proven by
    # arc_component_bodies_serialize_identically_to_vec_bodies in the same
    # file. CHECKPOINT_COMPONENT_ENCODING_VERSION stays 2.
    'crates/lash-core-store/src/store/checkpoint.rs:CHECKPOINT_COMPONENT_ENCODING_VERSION': 'sha256:5286fc29eca048a3426dc988c4250dfe488e562a7af994713a0077db012a88f2',
    # FIG-635: promise_key_preimage gained the tag-6 arm for the new
    # TurnCancelEscalation wait identity. The tag registry is append-only and
    # every previously issued key (tags 1-5 under an unchanged scope preimage)
    # is byte-identical; tag 6 only ever produces keys that never existed, so
    # AWAIT_EVENT_FAMILY_VERSION stays 3 (a bump would refuse every live
    # durable wait on every backend). Reviewer-confirmed one-time baseline.
    'crates/lash-core/src/runtime/effect/promise_semantics.rs:AWAIT_EVENT_FAMILY_VERSION': 'sha256:6a7905bb43b794600173e24507b7fd0369217a4828b65b624737ca7889ff1418',
    # FIG-2790: the turn identity became a transparent newtype over String
    # (`#[repr(transparent)]` + `#[serde(transparent)]`) and was adopted end to
    # end. The guards project guarded Rust TEXT, so every one of these six
    # surfaces reads a retyping as a shape change; not one serialized byte
    # moved. Two independent reviewers answered the byte-identity question per
    # surface, by name, before this entry was written, and neither found a
    # difference on any of the six.
    #
    # The residual is the one this dict always carries and is no narrower here:
    # the baseline pins a STATE, not a transition. Any future change that
    # restores a guarded surface to exactly these bytes would re-match and be
    # excused a second time, and any further guarded-shape drift on these
    # surfaces re-fails the gate and needs its own reading.
    # The replay-key preimage never reaches serde: `IdentityEncoder::string`
    # takes a concrete `&str`, so a `&TurnId` deref-coerces and the encoder
    # cannot see the newtype. Every hex preimage literal in the tests is
    # unchanged and the golden corpus passes on both sides.
    'crates/lash-core/src/runtime/causal.rs:DIRECT_EFFECT_FAMILY_VERSION': 'sha256:779f164af1b1046c3311d82c9ea1f1694f697dc0d345a301f164cd282f93217e',
    # The native envelope is encoded by `serde_json::to_value`; only
    # `Repair::turn_id` retypes, the `#[serde(tag)]` discriminants and field
    # order are untouched, and none of the 20 insta snapshots under
    # native/snapshots/ moved.
    'crates/lash-protocol-rlm/src/native/transport.rs:NATIVE_TRANSPORT_VERSION': 'sha256:47a1605ada731d8c6882cafb7afd09a9997165872537a0f952e9ee9f66779113',
    # FIG-2791: session and process identities became transparent newtypes,
    # following the turn identity of FIG-2790. Every guarded surface below
    # changed Rust shape only: the newtypes are #[repr(transparent)] and
    # #[serde(transparent)] over String, IdentityEncoder::string takes &str and
    # is reached by Deref, and derived Ord over a single String field is
    # String's Ord, so BTreeMap/BTreeSet durable ordering is preserved. Two
    # independent reviewers answered per surface by name, each with an executed
    # byte comparison rather than an argument: extracted definitions compiled on
    # both revisions emit identical output, and the workbench product-event log
    # round-trips byte-identically through its real decoder. Five keys above
    # were updated in place rather than appended -- a duplicate key in a Python
    # dict literal silently keeps the last one, which would leave a dead but
    # authoritative-looking baseline behind. As with every entry here, these pin
    # a STATE and not a transition: restoring exactly these bytes later would
    # re-match and be excused again.
    # FIG-3042 step 4 (live): the guarded ProcessWakeDelivery payload struct
    # moved verbatim from crates/lash-core/src/runtime/process/events.rs into
    # crates/lash-core-store/src/process_identity.rs, taking its derives and
    # serde attributes with it; the outbox writer and the constant itself stay
    # in events.rs, so this key keeps its merge-base identity while the guard
    # path follows the struct. The move changed no field, no serde attribute
    # and no derive input, so the serialized outbox payload is byte-identical
    # and PROCESS_WAKE_DELIVERY_FORMAT_VERSION stays 3. (Superseded state:
    # sha256:d918335cb663799309bcc9e97f4997719ec782481526c417bf09c012c1bc2316.)
    "crates/lash-core/src/runtime/process/events.rs:PROCESS_WAKE_DELIVERY_FORMAT_VERSION": (
        "sha256:1b3ccb158e7da7ad6bfe7a5c76d0029e7e589c991faece2ed89b4153dbd75a7e"
    ),
    "crates/lash-core/src/store/semantic_boundary.rs:RECORD_CONFIG_REQUEST_IDENTITY_ENCODING_VERSION": (
        "sha256:d3a77b92196da92208db28436247f96f29491dcc6e4012511649cfbb38e8c993"
    ),
    "crates/lash-core/src/store/semantic_boundary.rs:CREATE_SESSION_REQUEST_IDENTITY_ENCODING_VERSION": (
        "sha256:d3a77b92196da92208db28436247f96f29491dcc6e4012511649cfbb38e8c993"
    ),
    "crates/lash-core/src/store/semantic_boundary.rs:USAGE_LEDGER_REQUEST_IDENTITY_ENCODING_VERSION": (
        "sha256:d3a77b92196da92208db28436247f96f29491dcc6e4012511649cfbb38e8c993"
    ),
    # FIG-2828: the process-transfer preimage helper moved into the extracted
    # effect identity module. Its version constant stays at the original
    # envelope path so the checker retains merge-base identity; emitted bytes
    # remain pinned by the unchanged v1 golden.
    "crates/lash-core/src/runtime/effect/envelope.rs:PROCESS_TRANSFER_FAMILY_VERSION": (
        "sha256:d3b31f3bbd8eb783fca3d671c69fc55a2baefb988b37f5c1b546b91e43299b77"
    ),
    "crates/lash-core/src/runtime/process/events.rs:PROCESS_CANCELLATION_FAMILY_VERSION": (
        "sha256:55328d292629d8021ac81b8998697dcff2a998b9d9bc7842a0333ee31db2d7e8"
    ),
    "crates/lash-core/src/runtime/process/wake.rs:PROCESS_WAKE_FAMILY_VERSION": (
        "sha256:7f5f17a1dc9a67fba9be15a916ea5c1cddf7b6e2c497fb255a87973cba0e4db9"
    ),
    "crates/lash-core/src/store/mod.rs:SESSION_HEAD_META_SCHEMA_VERSION": (
        "sha256:816c0af2f5f70cdeeb31d3fad2b954bd4309f687e7dec72ce084a4e514a30c73"
    ),
    # FIG-3331 (live): the guarded crates/lash-core/src/runtime/process/engine.rs
    # moved verbatim to crates/lash-core-execution/src/runtime/process/engine.rs
    # (the guard path follows it); the only textual change is four inherent
    # constructors/converters widened from pub(crate) to pub for lash-core's
    # callers. SegmentHandover and PersistedSegmentHandover kept every field,
    # derive and serde attribute, so the persisted handover JSON is
    # byte-identical and LASHLANG_SEGMENT_STATE_VERSION stays 11. (Superseded
    # state: sha256:69b5d38f6c363f2cde541aa4500f5f31618b888826b44a4aa35cfe23a946bd1d.)
    "crates/lash-lashlang-runtime/src/process.rs:LASHLANG_SEGMENT_STATE_VERSION": (
        "sha256:b2b4caa01166c02528582a2d091a8d2ea5466b9faebaf735267ae72858c26dde"
    ),
    # FIG-2865: the canonical projected encoding moved out of state/wire.rs
    # into runtime/projected_wire.rs, which is now inside the snapshot guard's
    # globs, so the guarded text reads as a shape change. The snapshot bytes
    # are unchanged -- the relocated types serialize the same three fields in
    # the same order, and the independent review verified byte identity on
    # both sides -- so LASHLANG_SNAPSHOT_VERSION stays 7. The continuation
    # wire did change shape and took its honest bump
    # (BYTECODE_FORMAT_VERSION 13 -> 14, VM_CONTINUATION_FORMAT_VERSION
    # 11 -> 12). Any further guarded-shape drift re-fails the gate.
    "crates/lashlang/src/runtime/state.rs:LASHLANG_SNAPSHOT_VERSION": (
        "sha256:294f7111ef9bfc2d934213f174516de111d8dde72413fd0be1163f5fbd78ccee"
    ),
    # FIG-3020: `Span` moved verbatim from crates/lashlang/src/lexer.rs
    # to crates/lashlang/src/span.rs (the path in versioned-surfaces.toml
    # follows it) when the authored surface was deleted; same derives, same two
    # usize fields, no serde change, so the serialized bytes are identical and
    # VM_CONTINUATION_FORMAT_VERSION stayed 14.
    #
    # FIG-3469 (live) supersedes that reading after `AstString` replaced the
    # `CompactString` alias with a serde-transparent local wrapper. The
    # wrapper's byte-agreement test pins the same strings, so the continuation
    # remains byte-identical at version 16. Any further drift re-fails.
    #
    # FIG-3662 (live) supersedes again after `CyclicHostValue` gained a
    # `TS_CYCLIC_VALUE_UNSUPPORTED` prefix in its `#[error]` Display text.
    # Serde serializes the variant fields (`id`), not the rendered message, so
    # the continuation bytes are identical and the version stays 22.
    #
    # FIG-3653 supersedes again: `RuntimeError` gained the `EcmaThrow`
    # variant, which is `#[serde(skip)]`. The VM's error routing turns it into
    # a thrown error object before any handler or `finally` sees it, so it is
    # never a suspended finally's pending origin and has no wire form.
    # `continuation_runtime_error_wire_variants_are_pinned` passes unchanged:
    # the serialized variant vocabulary, and so the continuation bytes, are
    # identical, and the version stays 23.
    "crates/lashlang/src/runtime/vm/continuation.rs:"
    "VM_CONTINUATION_FORMAT_VERSION": (
        "sha256:4aed7dc999a0c19392c42d78a5197be8b5457b026fcaf44c9d26c0f342d8c035"
    ),
    # FIG-2992: a process identity's `definition` became the typed
    # `ProcessDefinitionRef` instead of a bare `serde_json::Value`. Both trigger
    # projections were retyped to match and nothing else: the draft projection
    # still writes the engine kind string and then the engine-owned definition
    # value alone (`reference.definition.as_json()`), and the subscription
    # filter still compares that same value. The engine kind is already fixed by
    # the projected `kind` leaf and the signature is a claim the engine resolves,
    # so neither joins the preimage. Regenerating the durable-read fixture on
    # both backends reproduced trigger-definition, trigger-command,
    # trigger-subscription and trigger-operation fingerprints byte-identical to
    # the merge-base rows, which is the evidence that the shape reading is a
    # retyping: TRIGGER_COMMAND_FAMILY_VERSION stays 5 and
    # TRIGGER_DEFINITION_FAMILY_VERSION stays 4.
    "crates/lash-core/src/triggers.rs:TRIGGER_COMMAND_FAMILY_VERSION": (
        "sha256:f14bf2e2d8b714b8d89f617652ae1586bb56d5d6718d41b45a827c10bf5f014b"
    ),
    "crates/lash-core/src/triggers/router.rs:TRIGGER_DEFINITION_FAMILY_VERSION": (
        "sha256:fb3d470b763cbd828e7df0bde4fd205be0d7cd077de30acd08d25c7b3ad2b73e"
    ),
    # FIG-3306 (live): the parked driver states' `error` / `terminal_finish`
    # field pair folded into `#[serde(flatten)] outcome: ParkedCellOutcome`
    # (protocol/state.rs, native/state.rs). ParkedCellOutcome writes the same
    # two keys with the same null-key emission the raw Option fields produced,
    # and refuses to decode a record carrying both -- a state the old pair
    # admitted but no writer ever produced. The trajectory-entry and
    # parked-state serialization tests confirm the stored bytes are identical,
    # so RLM_SNAPSHOT_VERSION stays 21 and NATIVE_DRIVER_STATE_VERSION stays 2.
    # Any further guarded-shape drift re-fails the gate.
    "crates/lash-protocol-rlm/src/executor/snapshot.rs:RLM_SNAPSHOT_VERSION": (
        "sha256:5077dcfc2a842813eca8b45088eb267164f49d268be559edc1a92002ec23ef8e"
    ),
    "crates/lash-protocol-rlm/src/native/state.rs:NATIVE_DRIVER_STATE_VERSION": (
        "sha256:5fba57c12525c184666bfd36859f770e35d754113e0d94b9e6c971f7fea3b03b"
    ),
    # FIG-3237 (live): each waiting variant's `effect_id` plus the
    # `#[serde(skip)]` delivery flag folded into one `EffectDelivery` record,
    # serialized transparently as the bare id under the variant's existing
    # `effect_id` key (machine_state.rs). The `status` field stays
    # runtime-only and deserializes to its `Pending` default, so checkpoint
    # bytes are identical on both sides and a restored checkpoint still
    # re-delivers; the checkpoint round-trip and redelivery tests confirm.
    # TURN_CHECKPOINT_SCHEMA_VERSION stays 3. Any further guarded-shape drift
    # re-fails the gate.
    "crates/lash-sansio/src/sansio/machine_state.rs:TURN_CHECKPOINT_SCHEMA_VERSION": (
        "sha256:6056583b23117ff129cf39d93b9407bb95b0d06bf6e495f633b147662da1ed79"
    ),
    # FIG-3484: RuntimeCommit's exhaustive destructure adds queued_run: _,
    # which semantic-boundary purity validation refuses before encoding.
    # SemanticBoundaryRequestIntent and the encoded fields/hash are unchanged.
    # Preserve the record-config/create-session/usage-ledger versions 3/3/5;
    # the unchanged golden corpus and regenerated SQLite/PostgreSQL fixtures
    # retain the same record-config hash at identity_encoding_version 3.
    # Supersedes the FIG-3230 ignored graph_base_leaf_node_id baseline:
    # sha256:093813b12037a2397006fc10b1936714a47f94a87b778e304c490cde50ea3ad2.
    # FIG-3531 (updated in place; a duplicate key would keep only the last):
    # the destructure also adds the ignored `undelivered_turn_input_claims`,
    # the withheld checkpoint claims a cancelled turn's commit releases for
    # the undelivered disposition, refused non-empty on a boundary commit.
    # The encoded fields, serde attributes and constants are unchanged, so
    # 3/3/5 stay. Superseded state:
    # sha256:48d26fb4f576abf84a952620b6431359e2e7210597e8763655dedab9dd66676d.
    "crates/lash-core-store/src/store/semantic_boundary.rs:RECORD_CONFIG_REQUEST_IDENTITY_ENCODING_VERSION": (
        "sha256:674711b54ee29d1eb2192e996a1f2ba5104043b37eaa7af2b8918e01d8014cd2"
    ),
    "crates/lash-core-store/src/store/semantic_boundary.rs:CREATE_SESSION_REQUEST_IDENTITY_ENCODING_VERSION": (
        "sha256:674711b54ee29d1eb2192e996a1f2ba5104043b37eaa7af2b8918e01d8014cd2"
    ),
    "crates/lash-core-store/src/store/semantic_boundary.rs:USAGE_LEDGER_REQUEST_IDENTITY_ENCODING_VERSION": (
        "sha256:674711b54ee29d1eb2192e996a1f2ba5104043b37eaa7af2b8918e01d8014cd2"
    ),
    # Comment-only sweep: the guarded-shape texts moved solely by deleting or
    # slimming comments; no schema statement, struct, field, serde attribute,
    # preimage byte expression, tag, or constant value changed, so
    # SCHEMA_VERSION stays 108 and TURN_CHECKPOINT_SCHEMA_VERSION stays 5.
    # FIG-3397 PR B supersedes this reading: the tool-batch command is deleted,
    # and with it six RuntimeErrorCode variants no build can raise
    # (runtime_effect_tool_batch_{call_id,call_replay,empty,id},
    # tool_batch_missing_result, tool_batch_result_count_mismatch). Deletion
    # only: every surviving code keeps its wire spelling, and a stored deleted
    # spelling still decodes verbatim as RuntimeErrorCode::ForeignCode and
    # re-encodes to the same bytes. No column or CHECK moved, so the stamp
    # stays (PostgreSQL 116, SQLite 75). Superseded:
    # sha256:2955b2a75bc5f7b21d5d90181e45998022dd1162ee3a0c499b8fc0bba2c47b2e.
    # FIG-3397 PR C supersedes this reading: two RuntimeErrorCode variants are
    # added (aggregate_await_unsettled, effect_group_opener_bound_exceeded).
    # Addition only: every existing code keeps its wire spelling, no stored row
    # can hold a new one before this build writes it, and an older reader
    # decodes a new spelling verbatim as RuntimeErrorCode::ForeignCode and
    # re-encodes it to the same bytes. No column or CHECK moved, so the stamp
    # stays (PostgreSQL 118, SQLite 77). Superseded:
    # sha256:5b44c05846f0c1d9c7fd96e375d3e6cac38a16a2b07ad2dd96fd6e7d2fe5a472.
    # FIG-3619 supersedes this reading (lead ruling, 2026-09-24): two
    # RuntimeErrorCode variants are added (session_state_version_unsupported,
    # session_state_version_newer_than_runtime), and RuntimeError gains a
    # #[serde(skip)] field carrying the refused generations in-process only.
    # Addition only: every existing code keeps its wire spelling, the stored
    # error shape is unchanged, and an older reader decodes a new spelling
    # verbatim as RuntimeErrorCode::ForeignCode and re-encodes it to the same
    # bytes (a_stored_session_state_refusal_reads_as_a_foreign_code_before_the_codes_existed).
    # No column or CHECK moved, so the stamp stays (PostgreSQL 121). Superseded:
    # sha256:6f5c10194bb742f138c6753a040b9d4f8b53258db52ddcba0a9ec4084caa29c1.
    'crates/lash-postgres-store/src/lib.rs:SCHEMA_VERSION': 'sha256:1c8a32db1aa89c2f8de14f4e75dc0b00fdb77dc8dcfe0b65bef70ff7e5832f88',
    'crates/lash-sansio/src/sansio/machine_state.rs:TURN_CHECKPOINT_SCHEMA_VERSION': 'sha256:c47b1c80a5d170e70556653319621cc219a493ba54bea2ccdee7317fc9f850b9',
    # FIG-3418: the guarded surface moved for two reasons, neither of which
    # reaches a serialized byte. `EffectOpener` gained a `schemars::JsonSchema`
    # derive (needed so the typed `ParentScope` can carry it inside a
    # versioned storage payload) and had a doc comment reworded; JsonSchema
    # emits no serde output and the enum's serde attributes, variants and
    # fields are untouched. `tool_provider.rs` moved only inside `mod tests`,
    # where `ParentScope::Process { .. }` became `ParentScope::process(..)` —
    # a constructor retype in test code that is not part of the request's
    # serialized closure at all. `ParentScope` itself is not a field type of
    # `ToolChildRequest` or anything it reaches. Every other guarded file is
    # byte-identical to the merge-base, so the tool-child request wire bytes
    # cannot have changed and TOOL_CHILD_REQUEST_VERSION stays 4.
    # One-time baseline; any further guarded-shape drift re-fails the gate.
    "crates/lash-core-execution/src/runtime/effect/tool_child.rs:TOOL_CHILD_REQUEST_VERSION": (
        "sha256:ea4860d85513148e0a596e26fd07fd262d07f542746df766146fb597870c8c00"
    ),
    # FIG-3538: the file-level serde-shape projection moved because
    # `turn_protocol.rs` gained `ProjectorTurnInputs` and an optional
    # `projector_turn_inputs` field on `ExecutionEnvironmentSync`. Both live on
    # the journaled sync-outcome side: `Response` is not serialized and the
    # `TurnCheckpoint` reach (MachineState, pending `Effect`s, messages,
    # events) never includes the sync payload or the host-supplied
    # `TurnMachineConfig`, so checkpoint bytes are identical on both sides.
    # The field serializes only into `runtime_effect_replay.outcome_json`, an
    # opaque carrier the SQL stores never type-decode, and is Option+default,
    # so outcome rows written before the field existed still decode.
    # TURN_CHECKPOINT_SCHEMA_VERSION stays 7. One-time baseline; any further
    # guarded-shape drift re-fails the gate.
    "crates/lash-sansio/src/sansio/machine_state.rs:TURN_CHECKPOINT_SCHEMA_VERSION": (
        "sha256:1966aff9d664ca490ceb551a1724f488939768c5396089c837eb2b71bab3ca0b"
    ),
    # FIG-3537: enrolling lash_runtime_turn_commits.result_json in the
    # payload-shape gate required `schemars::JsonSchema` derives across the
    # receipt's field-type closure, which reaches types these five surfaces
    # also guard. JsonSchema emits no serde output: every diff in the guarded
    # files is a derive-list addition or a derive-input-only attribute, no
    # field name, variant name, serde attribute, preimage byte expression,
    # tag or constant value changed, and the regenerated durable-read
    # fixtures reproduced the guarded payloads byte-identically. The
    # serialized bytes cannot have moved, so TOOL_SETTLEMENT_VERSION stays 5,
    # TOOL_PRESENTATION_VERSION stays 1, TOOL_ATTEMPT_CAPTURE_VERSION stays 4,
    # TURN_CHECKPOINT_SCHEMA_VERSION stays 7 and
    # SESSION_NODE_BODY_SCHEMA_VERSION stays 21. One-time baselines; any
    # further guarded-shape drift re-fails the gate.
    'crates/lash-core-execution/src/runtime/effect/tool_settlement.rs:TOOL_SETTLEMENT_VERSION': 'sha256:b9aa9d815a1f28148cfb01d460f8035fc52b3ed41c2120a3f487b64878f8f76c',
    'crates/lash-core-execution/src/runtime/effect/tool_presentation.rs:TOOL_PRESENTATION_VERSION': 'sha256:ebba20281bc0efbc99e2c254846d49dadcb6e95c82a0bf0ea78dc5a0a538f994',
    'crates/lash-core-execution/src/runtime/effect/tool_settlement.rs:TOOL_ATTEMPT_CAPTURE_VERSION': 'sha256:ff5d37cb5f1d9f7b19d8531c770332037abfaa263ea22655314cb4732f4a9cde',
    'crates/lash-sansio/src/sansio/machine_state.rs:TURN_CHECKPOINT_SCHEMA_VERSION': 'sha256:cc4a431dd40874aad1ce9addb160ad9702cefb1545e43b28eadc051bcfe8c664',
    # The same enrollment also moved `NonNegativeFiniteF64` and its impls from
    # `llm/types.rs` into `llm/types/non_negative_finite_f64.rs` to keep the
    # host file inside the production line budget. A module move re-spells no
    # field name, variant, serde attribute, or `schema_name`, and the type is
    # still swept whole through the guard's `paths`, so the serialized bytes
    # are identical on both sides.
    'crates/lash-core-store/src/session_graph.rs:SESSION_NODE_BODY_SCHEMA_VERSION': 'sha256:742dec920b986c908faa3df91a1d9bb06e02624af0e9e0c0c64674d5cdc6f0c3',
    # FIG-3537: the protocol-turn-options schemars mirror was renamed
    # ProtocolTurnOptionsWire -> ProtocolTurnOptionsSchemaWire so it no longer
    # collides with the local decode carrier of the same name inside the
    # manual Deserialize impl. The mirror feeds only the payload-shape
    # document; the persisted envelope is still emitted by the untouched
    # hand-written serialize/deserialize codecs, so the serialized bytes are
    # identical and PROTOCOL_TURN_OPTIONS_SCHEMA_VERSION stays 1.
    'crates/lash-core-store/src/protocol_turn_options.rs:PROTOCOL_TURN_OPTIONS_SCHEMA_VERSION': 'sha256:ad097b283e75c77ba0efeb14e03898e87e8f4673eb9eeb2a52c996c8fa42d01b',
    # FIG-3568: the durable-wait index gained the cancel-decided completion
    # fence (ADR 0099 §4, W17). Every existing encoding is byte-identical: the
    # index metadata's new `cancel_decided` set is `serde(default)` and
    # skipped while empty, and `LashDurableWaitIndex/resolve` answers an
    # untagged superset of `ResolveOutcome` whose outcome arm encodes exactly
    # as before, so recorded journals replay (the checked-in tool-intent
    # journal corpus holds one such answer). Only the refusal itself is new
    # bytes, so the epoch stays 6.
    'crates/lash-restate/src/durable_wait.rs:DURABLE_WAIT_INDEX_IDENTITY_EPOCH': 'sha256:e1ef76dded48453bc4dc1726a28bd365786116168b8723f7ae8966fd10b3cb2e',
}

# Burned one-time proofs that an atomic stack's lower branch already reserved
# the version carried by a guarded change on an upper branch. Unlike an
# identifier-rename baseline, this explicitly acknowledges changed serialized
# bytes. It is valid only while the stack lands atomically: the lower branch
# must not merge independently and publish the reserved version without the
# pinned upper-branch shape.
STACKED_VERSION_BASELINES = {
    # Protocol window 54 retires the window-53 pin. Layer 1 reserves 54;
    # FIG-1944 inline sources, FIG-2000 slot-free bodies, and FIG-2357 retained
    # host capability snapshots form one atomic train with this final shape.
    "crates/lash-remote-protocol/src/lib.rs:REMOTE_PROTOCOL_VERSION": (
        "sha256:d236866bcbb30109b344def73cefbd18912955ad79e8426b8a1a070343c431d2"
    ),
    # Schema generation 99 is the one bump the wholehog schema cluster
    # reserved on the lowest branch; FIG-2885 lands the session_meta family
    # CHECKs on an upper branch under this pinned final shape.
    "crates/lash-postgres-store/src/lib.rs:SCHEMA_VERSION": (
        "sha256:847eda650db5c2496bb98e8fdc86d19847e3d6b9b9ad13805e5fca2234ace549"
    ),
}


@dataclass(frozen=True)
class Guard:
    kind: str
    paths: tuple[str, ...]
    symbols: tuple[str, ...] = ()
    must_cover: tuple[str, ...] = ()
    elide: str | None = None


# A non-unique `CREATE INDEX IF NOT EXISTS ... ;` statement, whole.
#
# In a catalog whose open path always executes the entire schema text, such a
# statement is not a compatibility boundary: it reaches an existing database on
# its next open, and a database that already has the index stays readable by a
# binary whose schema omits it. Both directions are therefore compatible on one
# file, which is why a guarded surface may declare `elide` and let an
# index-only addition ship without a version bump.
#
# Elision applies only to statements that introduce NEW index names relative to
# the base revision. A statement whose index name already exists on the base side
# is a modification, not an addition: it is not elided, so altering an existing
# index's definition (such as its column list) demands a version bump. Existing
# index statements on the base side are also kept in the base signature, so
# removing an index deliberately demands a version bump. `UNIQUE` is deliberately
# not matched: a unique index is a constraint, `IF NOT EXISTS` will not
# re-create a differently-shaped one, and it must keep demanding a bump.
IDEMPOTENT_SQL_INDEX = re.compile(
    r"\s*CREATE\s+INDEX\s+IF\s+NOT\s+EXISTS\s+([^\s(]+)\s+ON\b.*?;",
    re.DOTALL | re.IGNORECASE,
)


def normalize_sql_index_name(name: str) -> str:
    return name.strip('"`[]').lower()


def extract_idempotent_sql_index_names(text: str) -> set[str]:
    return {
        normalize_sql_index_name(match.group(1))
        for match in IDEMPOTENT_SQL_INDEX.finditer(text)
    }


def elide_new_sql_indexes(head_value: str, base_value: str = "") -> str:
    base_indexes = extract_idempotent_sql_index_names(base_value)

    def replacer(match: re.Match[str]) -> str:
        name = normalize_sql_index_name(match.group(1))
        if name not in base_indexes:
            return ""
        return match.group(0)

    return IDEMPOTENT_SQL_INDEX.sub(replacer, head_value)


ELISIONS = {
    "sql_idempotent_index": elide_new_sql_indexes,
}


@dataclass(frozen=True)
class Surface:
    constant: str
    constant_path: str
    description: str
    guards: tuple[Guard, ...]
    version_regex: str | None = None

    @property
    def key(self) -> str:
        return f"{self.constant_path}:{self.constant}"


@dataclass(frozen=True)
class Failure:
    surface: Surface
    base_version: int
    head_version: int
    changed_guards: tuple[str, ...]
    fingerprint: str = ""


@dataclass(frozen=True)
class SurfaceError:
    surface: Surface
    detail: str


@dataclass(frozen=True)
class Unregistered:
    """A changed shape whose constant has no merge-base value and no baseline.

    This is what a renamed or relocated version constant looks like from the
    gate's side, so it is reported as a failure rather than an undecidable
    surface: the shape moved, and nothing in the inventory can say what it was
    supposed to move past.
    """

    surface: Surface
    changed_guards: tuple[str, ...]
    fingerprint: str


@dataclass(frozen=True)
class CheckResult:
    failures: tuple[Failure, ...]
    errors: tuple[SurfaceError, ...]
    registrations: tuple[Surface, ...] = ()
    unregistered: tuple[Unregistered, ...] = ()
    identifier_renames: tuple[Surface, ...] = ()
    stacked_versions: tuple[Surface, ...] = ()


class CheckError(RuntimeError):
    """Configuration, repository, or source-shape error."""


def git(repo: Path, *args: str, check: bool = True) -> subprocess.CompletedProcess[str]:
    # Captured as bytes and decoded here rather than through subprocess's text
    # mode, for two reasons that both matter to the guard signatures built from
    # this output. Strict UTF-8 cannot read the binary durable-read fixtures at
    # all, so decoding is lenient -- but lenient as `surrogateescape`, which
    # round-trips every byte to a distinct code point, never `replace`, which
    # collapses every invalid byte to one U+FFFD and would render a changed
    # binary fixture identical to the original. And text mode also applies
    # universal-newline translation, which folds CR and CRLF into LF: in a
    # binary payload that is a real content change reading as no change.
    completed = subprocess.run(["git", *args], cwd=repo, capture_output=True)
    result: subprocess.CompletedProcess[str] = subprocess.CompletedProcess(
        completed.args,
        completed.returncode,
        completed.stdout.decode("utf-8", errors="surrogateescape"),
        completed.stderr.decode("utf-8", errors="surrogateescape"),
    )
    if check and result.returncode != 0:
        detail = result.stderr.strip() or result.stdout.strip()
        raise CheckError(f"git {' '.join(args)} failed: {detail}")
    return result


def resolve_revision(repo: Path, revision: str) -> str:
    return git(repo, "rev-parse", "--verify", f"{revision}^{{commit}}").stdout.strip()


def load_config(path: Path) -> tuple[Surface, ...]:
    try:
        document = tomllib.loads(path.read_text(encoding="utf-8"))
    except (OSError, tomllib.TOMLDecodeError) as error:
        raise CheckError(f"cannot read {path}: {error}") from error

    raw_surfaces = document.get("surface")
    if not isinstance(raw_surfaces, list) or not raw_surfaces:
        raise CheckError(f"{path}: expected at least one [[surface]] entry")

    surfaces: list[Surface] = []
    seen: set[tuple[str, str]] = set()
    for index, raw_surface in enumerate(raw_surfaces, start=1):
        location = f"{path}: surface {index}"
        if not isinstance(raw_surface, dict):
            raise CheckError(f"{location} must be a table")
        try:
            constant = raw_surface["constant"]
            constant_path = raw_surface["constant_path"]
            description = raw_surface["description"]
            raw_guards = raw_surface["guard"]
        except KeyError as error:
            raise CheckError(f"{location} is missing {error.args[0]}") from error
        if not all(
            isinstance(value, str) and value
            for value in (constant, constant_path, description)
        ):
            raise CheckError(f"{location} has an empty or non-string required field")
        identity = (constant_path, constant)
        if identity in seen:
            raise CheckError(f"{location} duplicates {constant} in {constant_path}")
        seen.add(identity)
        if not isinstance(raw_guards, list) or not raw_guards:
            raise CheckError(f"{location} must contain at least one [[surface.guard]]")

        guards: list[Guard] = []
        for guard_index, raw_guard in enumerate(raw_guards, start=1):
            guard_location = f"{location}, guard {guard_index}"
            if not isinstance(raw_guard, dict):
                raise CheckError(f"{guard_location} must be a table")
            kind = raw_guard.get("kind")
            paths = raw_guard.get("paths")
            symbols = raw_guard.get("symbols", [])
            must_cover = raw_guard.get("must_cover", [])
            if kind not in {
                "file",
                "rust_items",
                "rust_impls",
                "rust_serde_shapes",
            }:
                raise CheckError(f"{guard_location} has unsupported kind {kind!r}")
            if not isinstance(paths, list) or not paths or not all(
                isinstance(value, str) and value for value in paths
            ):
                raise CheckError(f"{guard_location} paths must be non-empty strings")
            if not isinstance(symbols, list) or not all(
                isinstance(value, str) and value for value in symbols
            ):
                raise CheckError(f"{guard_location} symbols must be strings")
            if not isinstance(must_cover, list) or not all(
                isinstance(value, str) and value for value in must_cover
            ):
                raise CheckError(f"{guard_location} must_cover must be strings")
            if kind in {"rust_items", "rust_impls"} and not symbols:
                raise CheckError(f"{guard_location} {kind} requires symbols")
            if kind not in {"rust_items", "rust_impls"} and symbols:
                raise CheckError(
                    f"{guard_location} only rust_items and rust_impls accept symbols"
                )
            if kind == "rust_serde_shapes" and not must_cover:
                raise CheckError(
                    f"{guard_location} rust_serde_shapes requires must_cover"
                )
            if kind not in {"file", "rust_serde_shapes"} and must_cover:
                raise CheckError(
                    f"{guard_location} only file and rust_serde_shapes accept "
                    "must_cover"
                )
            if len(must_cover) != len(set(must_cover)):
                raise CheckError(f"{guard_location} must_cover contains duplicates")
            elide = raw_guard.get("elide")
            if elide is not None and elide not in ELISIONS:
                raise CheckError(
                    f"{guard_location} has unsupported elide {elide!r}; known: "
                    + ", ".join(sorted(ELISIONS))
                )
            guards.append(
                Guard(kind, tuple(paths), tuple(symbols), tuple(must_cover), elide)
            )

        version_regex = raw_surface.get("version_regex")
        if version_regex is not None and not isinstance(version_regex, str):
            raise CheckError(f"{location} version_regex must be a string")
        surfaces.append(
            Surface(
                constant=constant,
                constant_path=constant_path,
                description=description,
                guards=tuple(guards),
                version_regex=version_regex,
            )
        )
    return tuple(surfaces)


class RepositoryView:
    def __init__(self, repo: Path) -> None:
        self.repo = repo
        self._trees: dict[str, tuple[str, ...]] = {}
        self._contents: dict[tuple[str, str], str | None] = {}

    def paths(self, revision: str) -> tuple[str, ...]:
        if revision not in self._trees:
            output = git(self.repo, "ls-tree", "-r", "--name-only", revision).stdout
            self._trees[revision] = tuple(line for line in output.splitlines() if line)
        return self._trees[revision]

    def matching_paths(self, revision: str, patterns: Iterable[str]) -> tuple[str, ...]:
        matches = {
            path
            for path in self.paths(revision)
            for pattern in patterns
            if fnmatch.fnmatchcase(path, pattern)
        }
        return tuple(sorted(matches))

    def content(self, revision: str, path: str) -> str | None:
        key = (revision, path)
        if key not in self._contents:
            result = git(self.repo, "show", f"{revision}:{path}", check=False)
            self._contents[key] = result.stdout if result.returncode == 0 else None
        return self._contents[key]


def _raw_string_start(text: str, index: int) -> tuple[int, str] | None:
    prefix = index
    if text.startswith("br", index):
        prefix += 1
    if not text.startswith("r", prefix):
        return None
    cursor = prefix + 1
    while cursor < len(text) and text[cursor] == "#":
        cursor += 1
    if cursor >= len(text) or text[cursor] != '"':
        return None
    hashes = text[prefix + 1 : cursor]
    return cursor + 1, '"' + hashes


def _char_literal_end(text: str, index: int) -> int | None:
    quote = index + 1 if text.startswith("b'", index) else index
    if quote >= len(text) or text[quote] != "'" or quote + 1 >= len(text):
        return None
    cursor = quote + 1
    if text[cursor] == "\\":
        cursor += 2
        while cursor < len(text):
            if text[cursor] == "'":
                return cursor + 1
            cursor += 2 if text[cursor] == "\\" else 1
        return None
    if cursor + 1 < len(text) and text[cursor + 1] == "'":
        return cursor + 2
    return None


def rust_item_end(text: str, start: int) -> int:
    """Return the end of one Rust item, ignoring delimiters inside literals.

    All three delimiter kinds are tracked, not braces alone, because an item's
    terminator is only its terminator at the top level of the item. A brace
    inside a bracket is a struct literal in an initializer -- the registry
    constant ``const XS: &[Builtin] = &[Builtin { .. }, ..];`` is the shape that
    matters here -- and a semicolon inside a bracket is an array length. Reading
    either as the end truncates the item after its first element, which reads as
    a guard over a table while covering only its head: appending to the table
    then changes nothing the guard can see.
    """
    index = start
    brace_depth = 0
    bracket_depth = 0
    paren_depth = 0
    saw_brace = False
    block_comment_depth = 0
    state = "normal"
    raw_closer = ""
    while index < len(text):
        char = text[index]
        following = text[index + 1] if index + 1 < len(text) else ""
        if state == "line_comment":
            if char == "\n":
                state = "normal"
            index += 1
            continue
        if state == "block_comment":
            if char == "/" and following == "*":
                block_comment_depth += 1
                index += 2
            elif char == "*" and following == "/":
                block_comment_depth -= 1
                index += 2
                if block_comment_depth == 0:
                    state = "normal"
            else:
                index += 1
            continue
        if state == "string":
            if char == "\\":
                index += 2
            else:
                index += 1
                if char == '"':
                    state = "normal"
            continue
        if state == "char":
            if char == "\\":
                index += 2
            else:
                index += 1
                if char == "'":
                    state = "normal"
            continue
        if state == "raw":
            closing = text.find(raw_closer, index)
            if closing < 0:
                raise CheckError(
                    "unterminated Rust raw string while extracting guarded item"
                )
            index = closing + len(raw_closer)
            state = "normal"
            continue

        if char == "/" and following == "/":
            state = "line_comment"
            index += 2
        elif char == "/" and following == "*":
            state = "block_comment"
            block_comment_depth = 1
            index += 2
        elif raw := _raw_string_start(text, index):
            index, raw_closer = raw
            state = "raw"
        elif char == '"' or (char == "b" and following == '"'):
            state = "string"
            index += 2 if char == "b" else 1
        elif char_end := _char_literal_end(text, index):
            index = char_end
        elif char == "{":
            if brace_depth == 0 and bracket_depth == 0 and paren_depth == 0:
                saw_brace = True
            brace_depth += 1
            index += 1
        elif char == "}":
            brace_depth -= 1
            index += 1
            if saw_brace and brace_depth == 0:
                return index
        elif char == "[":
            bracket_depth += 1
            index += 1
        elif char == "]":
            bracket_depth -= 1
            index += 1
        elif char == "(":
            paren_depth += 1
            index += 1
        elif char == ")":
            paren_depth -= 1
            index += 1
        elif char == ";" and not brace_depth and not bracket_depth and not paren_depth:
            return index + 1
        else:
            index += 1
    raise CheckError("unterminated Rust item while extracting guarded shape")


def strip_rust_trivia(text: str) -> str:
    """Remove Rust whitespace/comments while preserving literals and tokens."""
    output: list[str] = []
    index = 0
    block_depth = 0
    while index < len(text):
        char = text[index]
        following = text[index + 1] if index + 1 < len(text) else ""
        if char.isspace():
            index += 1
        elif char == "/" and following == "/":
            newline = text.find("\n", index + 2)
            index = len(text) if newline < 0 else newline + 1
        elif char == "/" and following == "*":
            index += 2
            block_depth = 1
            while index < len(text) and block_depth:
                if text.startswith("/*", index):
                    block_depth += 1
                    index += 2
                elif text.startswith("*/", index):
                    block_depth -= 1
                    index += 2
                else:
                    index += 1
        elif raw := _raw_string_start(text, index):
            content_start, closer = raw
            closing = text.find(closer, content_start)
            if closing < 0:
                raise CheckError(
                    "unterminated Rust raw string while normalizing guarded item"
                )
            end = closing + len(closer)
            output.append(text[index:end])
            index = end
        elif char == '"' or (char == "b" and following == '"'):
            start = index
            index += 2 if char == "b" else 1
            while index < len(text):
                if text[index] == "\\":
                    index += 2
                else:
                    closing = text[index] == '"'
                    index += 1
                    if closing:
                        break
            output.append(text[start:index])
        elif char_end := _char_literal_end(text, index):
            output.append(text[index:char_end])
            index = char_end
        else:
            output.append(char)
            index += 1
    return "".join(output)


def rust_attribute_end(text: str, start: int) -> int:
    """Return the end of one balanced Rust outer attribute."""
    if not text.startswith("#[", start):
        raise CheckError("Rust attribute extraction did not start at #[")
    index = start + 2
    bracket_depth = 1
    block_comment_depth = 0
    state = "normal"
    raw_closer = ""
    while index < len(text):
        char = text[index]
        following = text[index + 1] if index + 1 < len(text) else ""
        if state == "line_comment":
            if char == "\n":
                state = "normal"
            index += 1
            continue
        if state == "block_comment":
            if char == "/" and following == "*":
                block_comment_depth += 1
                index += 2
            elif char == "*" and following == "/":
                block_comment_depth -= 1
                index += 2
                if block_comment_depth == 0:
                    state = "normal"
            else:
                index += 1
            continue
        if state == "string":
            if char == "\\":
                index += 2
            else:
                index += 1
                if char == '"':
                    state = "normal"
            continue
        if state == "raw":
            closing = text.find(raw_closer, index)
            if closing < 0:
                raise CheckError(
                    "unterminated Rust raw string while extracting attribute"
                )
            index = closing + len(raw_closer)
            state = "normal"
            continue

        if char == "/" and following == "/":
            state = "line_comment"
            index += 2
        elif char == "/" and following == "*":
            state = "block_comment"
            block_comment_depth = 1
            index += 2
        elif raw := _raw_string_start(text, index):
            index, raw_closer = raw
            state = "raw"
        elif char == '"' or (char == "b" and following == '"'):
            state = "string"
            index += 2 if char == "b" else 1
        elif char_end := _char_literal_end(text, index):
            index = char_end
        elif char == "[":
            bracket_depth += 1
            index += 1
        elif char == "]":
            bracket_depth -= 1
            index += 1
            if bracket_depth == 0:
                return index
        else:
            index += 1
    raise CheckError("unterminated Rust outer attribute")


def _raw_outer_attribute_ranges(text: str, start: int) -> tuple[tuple[int, int], ...]:
    """Conservatively recover attributes after malformed Rust trivia."""
    ranges: list[tuple[int, int]] = []
    cursor = start
    while True:
        attribute_start = text.find("#[", cursor)
        if attribute_start < 0:
            return tuple(ranges)
        try:
            attribute_end = rust_attribute_end(text, attribute_start)
        except CheckError:
            cursor = attribute_start + 2
        else:
            ranges.append((attribute_start, attribute_end))
            cursor = attribute_end


def rust_outer_attribute_ranges(text: str) -> tuple[tuple[int, int], ...]:
    """Find outer attributes while ignoring attribute-looking text in trivia."""
    ranges: list[tuple[int, int]] = []
    index = 0
    while index < len(text):
        following = text[index + 1] if index + 1 < len(text) else ""
        if text.startswith("#[", index):
            end = rust_attribute_end(text, index)
            ranges.append((index, end))
            index = end
        elif text[index] == "/" and following == "/":
            newline = text.find("\n", index + 2)
            index = len(text) if newline < 0 else newline + 1
        elif text[index] == "/" and following == "*":
            malformed_start = index + 2
            index += 2
            block_depth = 1
            while index < len(text) and block_depth:
                if text.startswith("/*", index):
                    block_depth += 1
                    index += 2
                elif text.startswith("*/", index):
                    block_depth -= 1
                    index += 2
                else:
                    index += 1
            if block_depth:
                ranges.extend(_raw_outer_attribute_ranges(text, malformed_start))
                return tuple(ranges)
        elif raw := _raw_string_start(text, index):
            content_start, closer = raw
            closing = text.find(closer, content_start)
            if closing < 0:
                ranges.extend(_raw_outer_attribute_ranges(text, content_start))
                return tuple(ranges)
            index = closing + len(closer)
        elif text[index] == '"' or (text[index] == "b" and following == '"'):
            malformed_start = index + (2 if text[index] == "b" else 1)
            index += 2 if text[index] == "b" else 1
            closed = False
            while index < len(text):
                if text[index] == "\\":
                    index += 2
                else:
                    closing = text[index] == '"'
                    index += 1
                    if closing:
                        closed = True
                        break
            if not closed:
                ranges.extend(_raw_outer_attribute_ranges(text, malformed_start))
                return tuple(ranges)
        elif char_end := _char_literal_end(text, index):
            index = char_end
        else:
            index += 1
    return tuple(ranges)


def rust_item_start_with_attributes(
    text: str,
    declaration_start: int,
    attribute_ranges: tuple[tuple[int, int], ...] | None = None,
) -> int:
    """Walk back across complete attributes and interleaved Rust comments."""
    line_start = text.rfind("\n", 0, declaration_start) + 1
    cursor = line_start
    ranges = attribute_ranges or rust_outer_attribute_ranges(text[:declaration_start])
    eligible = [attribute for attribute in ranges if attribute[1] <= cursor]
    while eligible:
        start, end = eligible[-1]
        if strip_rust_trivia(text[end:cursor]):
            break
        cursor = start
        eligible.pop()
    return cursor


NON_WIRE_DERIVES = {
    "Debug",
    "Clone",
    "Copy",
    "PartialEq",
    "Eq",
    "PartialOrd",
    "Ord",
    "Hash",
    "Default",
    "schemars::JsonSchema",
    "thiserror::Error",
}


def _rust_group_end(text: str, open_index: int) -> int | None:
    """Return the exclusive end of a token-aware parenthesized Rust group."""
    if open_index >= len(text) or text[open_index] != "(":
        return None
    index = open_index + 1
    depth = 1
    while index < len(text):
        following = text[index + 1] if index + 1 < len(text) else ""
        if text.startswith("//", index):
            newline = text.find("\n", index + 2)
            index = len(text) if newline < 0 else newline + 1
        elif text.startswith("/*", index):
            skipped = _rust_trivia_end(text, index)
            if skipped is None:
                return None
            index = skipped
        elif raw := _raw_string_start(text, index):
            content_start, closer = raw
            closing = text.find(closer, content_start)
            if closing < 0:
                return None
            index = closing + len(closer)
        elif text[index] == '"' or (text[index] == "b" and following == '"'):
            index += 2 if text[index] == "b" else 1
            while index < len(text):
                if text[index] == "\\":
                    index += 2
                else:
                    closing = text[index] == '"'
                    index += 1
                    if closing:
                        break
            else:
                return None
        elif char_end := _char_literal_end(text, index):
            index = char_end
        elif text[index] == "(":
            depth += 1
            index += 1
        elif text[index] == ")":
            depth -= 1
            index += 1
            if depth == 0:
                return index
        else:
            index += 1
    return None


def _rust_top_level_items(text: str, start: int, end: int) -> tuple[tuple[int, int], ...] | None:
    """Split a Rust token range on top-level commas."""
    items: list[tuple[int, int]] = []
    item_start = start
    index = start
    delimiters: list[str] = []
    pairs = {"(": ")", "[": "]", "{": "}"}
    while index < end:
        following = text[index + 1] if index + 1 < end else ""
        if text.startswith("//", index):
            newline = text.find("\n", index + 2, end)
            index = end if newline < 0 else newline + 1
        elif text.startswith("/*", index):
            skipped = _rust_trivia_end(text[:end], index)
            if skipped is None:
                return None
            index = skipped
        elif raw := _raw_string_start(text, index):
            content_start, closer = raw
            closing = text.find(closer, content_start, end)
            if closing < 0:
                return None
            index = closing + len(closer)
        elif text[index] == '"' or (text[index] == "b" and following == '"'):
            index += 2 if text[index] == "b" else 1
            while index < end:
                if text[index] == "\\":
                    index += 2
                else:
                    closing = text[index] == '"'
                    index += 1
                    if closing:
                        break
            else:
                return None
        elif char_end := _char_literal_end(text, index):
            index = char_end
        elif text[index] in pairs:
            delimiters.append(pairs[text[index]])
            index += 1
        elif text[index] in ")]}":
            if not delimiters or delimiters.pop() != text[index]:
                return None
            index += 1
        elif text[index] == "," and not delimiters:
            items.append((item_start, index))
            item_start = index + 1
            index += 1
        else:
            index += 1
    if delimiters:
        return None
    items.append((item_start, end))
    return tuple(items)


def _derive_contents(text: str, start: int, end: int) -> tuple[int, int] | None:
    """Return contents for an item that is exactly `derive(...)`."""
    cursor = _rust_trivia_end(text, start)
    if cursor is None:
        return None
    name = re.match(r"derive\b", text[cursor:end])
    if name is None:
        return None
    cursor = _rust_trivia_end(text, cursor + len(name.group(0)))
    if cursor is None or cursor >= end or text[cursor] != "(":
        return None
    group_end = _rust_group_end(text[:end], cursor)
    if group_end is None:
        return None
    tail = _rust_trivia_end(text, group_end)
    if tail != end:
        return None
    return cursor + 1, group_end - 1


def _attribute_derive_lists(attribute: str) -> tuple[tuple[int, int], ...]:
    """Find only top-level derive and cfg_attr derive-list positions."""
    content_start = 2
    content_end = len(attribute) - 1
    cursor = _rust_trivia_end(attribute, content_start)
    if cursor is None:
        return ()
    name = re.match(r"([A-Za-z_][A-Za-z0-9_]*)\b", attribute[cursor:content_end])
    if name is None:
        return ()
    attribute_name = name.group(1)
    cursor = _rust_trivia_end(attribute, cursor + len(attribute_name))
    if cursor is None or cursor >= content_end or attribute[cursor] != "(":
        return ()
    group_end = _rust_group_end(attribute[:content_end], cursor)
    if group_end is None or _rust_trivia_end(attribute, group_end) != content_end:
        return ()
    if attribute_name == "derive":
        return ((cursor + 1, group_end - 1),)
    if attribute_name != "cfg_attr":
        return ()
    items = _rust_top_level_items(attribute, cursor + 1, group_end - 1)
    if items is None:
        return ()
    return tuple(
        contents
        for item_start, item_end in items[1:]
        if (contents := _derive_contents(attribute, item_start, item_end)) is not None
    )


def normalize_rust_derive_lists(text: str) -> str:
    """Ignore allowlisted non-wire derives in real derive attributes only."""
    replacements: list[tuple[int, int, str]] = []
    for attribute_start, attribute_end in rust_outer_attribute_ranges(text):
        attribute = text[attribute_start:attribute_end]
        for start, end in _attribute_derive_lists(attribute):
            items = _rust_top_level_items(attribute, start, end)
            if items is None:
                continue
            retained = [
                strip_rust_trivia(attribute[item_start:item_end])
                for item_start, item_end in items
                if strip_rust_trivia(attribute[item_start:item_end])
                and strip_rust_trivia(attribute[item_start:item_end]) not in NON_WIRE_DERIVES
            ]
            replacements.append(
                (attribute_start + start, attribute_start + end, ",".join(retained))
            )
    normalized = text
    for start, end, replacement in reversed(replacements):
        normalized = normalized[:start] + replacement + normalized[end:]
    return normalized


RUST_DECLARATION = re.compile(
    r"(?m)^[ \t]*(?:pub(?:\([^)]*\))?[ \t]+)?(?:async[ \t]+)?"
    r"(?:const|static|fn|struct|enum|type)[ \t]+([A-Za-z_][A-Za-z0-9_]*)\b"
)
RUST_SERDE_SHAPE = re.compile(
    r"(?m)^[ \t]*(?:pub(?:\([^)]*\))?[ \t]+)?"
    r"(?:struct|enum)[ \t]+([A-Za-z_][A-Za-z0-9_]*)\b"
)
RUST_SERDE_IMPL = re.compile(
    r"(?m)^[ \t]*impl(?:[ \t]*<[^>{}]*>)?[ \t]+"
    r"((?:serde::)?(?:Serialize|Deserialize)(?:[ \t]*<[^>{}]*>)?)"
    r"[ \t]+for[ \t]+([A-Za-z_][A-Za-z0-9_]*)\b"
)
RUST_INLINE_MODULE = re.compile(
    r"(?:pub(?:\s*\(\s*(?:crate|self|super|in\s+(?:crate|self|super)"
    r"(?:::[A-Za-z_][A-Za-z0-9_]*)*)\s*\))?\s+)?"
    r"mod\s+[A-Za-z_][A-Za-z0-9_]*\s*\{",
    re.ASCII,
)


_CFG_ASCII_SPACE = " \t\r\n"
_CFG_SIMPLE_STRING = re.compile(r'"[A-Za-z0-9_.-]*"')


def _rust_space_end(text: str, start: int) -> int:
    cursor = start
    while cursor < len(text) and text[cursor] in _CFG_ASCII_SPACE:
        cursor += 1
    return cursor


def _rust_string_end(text: str, start: int) -> int | None:
    """Return one simple cfg string's end, refusing anything with escapes.

    Proving a predicate test-only never requires interpreting Rust escape
    semantics, so instead of validating escapes this accepts only the plain
    identifier-like strings real cfg values use. Any other string leaves the
    predicate unproven and the region swept.
    """
    matched = _CFG_SIMPLE_STRING.match(text, start)
    return None if matched is None else matched.end()


def _cfg_predicate(text: str, start: int = 0) -> tuple[int, bool] | None:
    """Strictly parse enough cfg grammar to prove a predicate requires `test`."""
    cursor = _rust_space_end(text, start)
    identifier = re.match(r"[A-Za-z_][A-Za-z0-9_]*", text[cursor:])
    if identifier is None:
        return None
    name = identifier.group(0)
    cursor = _rust_space_end(text, cursor + len(name))
    if cursor == len(text) or text[cursor] in ",)":
        return cursor, name == "test"
    if text[cursor] == "=":
        cursor = _rust_space_end(text, cursor + 1)
        string_end = _rust_string_end(text, cursor)
        if string_end is None:
            return None
        return _rust_space_end(text, string_end), False
    if text[cursor] != "(" or name not in {"all", "any", "not"}:
        return None

    cursor = _rust_space_end(text, cursor + 1)
    children: list[bool] = []
    while cursor < len(text) and text[cursor] != ")":
        child = _cfg_predicate(text, cursor)
        if child is None:
            return None
        cursor, test_only = child
        children.append(test_only)
        if cursor < len(text) and text[cursor] == ",":
            cursor = _rust_space_end(text, cursor + 1)
        elif cursor >= len(text) or text[cursor] != ")":
            return None
    if cursor >= len(text) or text[cursor] != ")":
        return None
    if name == "not" and len(children) != 1:
        return None
    if name == "all":
        test_only = any(children)
    elif name == "any":
        test_only = bool(children) and all(children)
    else:
        test_only = False
    return cursor + 1, test_only


def _test_only_cfg(attribute: str) -> bool:
    cursor = _rust_space_end(attribute, 0)
    if cursor >= len(attribute) or attribute[cursor] != "#":
        return False
    cursor = _rust_space_end(attribute, cursor + 1)
    if cursor >= len(attribute) or attribute[cursor] != "[":
        return False
    cursor = _rust_space_end(attribute, cursor + 1)
    cfg = re.match(r"cfg\b", attribute[cursor:])
    if cfg is None:
        return False
    cursor = _rust_space_end(attribute, cursor + len(cfg.group(0)))
    if cursor >= len(attribute) or attribute[cursor] != "(":
        return False
    parsed = _cfg_predicate(attribute, cursor + 1)
    if parsed is None:
        return False
    cursor, test_only = parsed
    if cursor >= len(attribute) or attribute[cursor] != ")":
        return False
    cursor = _rust_space_end(attribute, cursor + 1)
    if cursor >= len(attribute) or attribute[cursor] != "]":
        return False
    cursor = _rust_space_end(attribute, cursor + 1)
    return cursor == len(attribute) and test_only


def _rust_trivia_end(text: str, start: int) -> int | None:
    """Skip whitespace and comments, returning None for malformed trivia."""
    cursor = start
    while cursor < len(text):
        if text[cursor] in _CFG_ASCII_SPACE:
            cursor += 1
        elif text.startswith("//", cursor):
            newline = text.find("\n", cursor + 2)
            cursor = len(text) if newline < 0 else newline + 1
        elif text.startswith("/*", cursor):
            cursor += 2
            depth = 1
            while cursor < len(text) and depth:
                if text.startswith("/*", cursor):
                    depth += 1
                    cursor += 2
                elif text.startswith("*/", cursor):
                    depth -= 1
                    cursor += 2
                else:
                    cursor += 1
            if depth:
                return None
        else:
            break
    return cursor


def test_only_module_ranges(
    text: str, attribute_ranges: tuple[tuple[int, int], ...]
) -> tuple[tuple[int, int], ...]:
    """Return bodies of inline modules that are certainly gated on `test`."""
    ranges: list[tuple[int, int]] = []
    attributes_by_start = {start: end for start, end in attribute_ranges}
    for start, end in attribute_ranges:
        if not _test_only_cfg(text[start:end]):
            continue
        cursor = end
        while True:
            cursor = _rust_trivia_end(text, cursor)
            if cursor is None:
                break
            next_attribute = attributes_by_start.get(cursor)
            if next_attribute is None:
                break
            cursor = next_attribute
        if cursor is None:
            continue
        module = RUST_INLINE_MODULE.match(text, cursor)
        if module is None:
            continue
        try:
            item_end = rust_item_end(text, cursor)
        except CheckError:
            continue
        if text[item_end - 1] == "}":
            ranges.append((module.end(), item_end - 1))
    return tuple(ranges)


def named_rust_items(text: str, names: Iterable[str]) -> dict[str, str]:
    wanted = set(names)
    found: dict[str, str] = {}
    attribute_ranges = rust_outer_attribute_ranges(text)
    for match in RUST_DECLARATION.finditer(text):
        name = match.group(1)
        if name not in wanted:
            continue
        start = rust_item_start_with_attributes(text, match.start(), attribute_ranges)
        end = rust_item_end(text, match.start())
        value = strip_rust_trivia(normalize_rust_derive_lists(text[start:end]))
        if name in found and found[name] != value:
            raise CheckError(f"guarded Rust symbol {name} is ambiguous in one file")
        found[name] = value
    return found


def named_rust_serde_impls(text: str, names: Iterable[str]) -> dict[str, str]:
    wanted = set(names)
    found: dict[str, str] = {}
    for match in RUST_SERDE_IMPL.finditer(text):
        trait = "Deserialize" if "Deserialize" in match.group(1) else "Serialize"
        name = f"{trait} for {match.group(2)}"
        if name not in wanted:
            continue
        end = rust_item_end(text, match.start())
        value = strip_rust_trivia(text[match.start() : end])
        if name in found and found[name] != value:
            raise CheckError(f"guarded Rust impl {name} is ambiguous in one file")
        found[name] = value
    return found


def serde_shapes(text: str) -> dict[str, str]:
    found: dict[str, str] = {}
    attribute_ranges = rust_outer_attribute_ranges(text)
    excluded_ranges = test_only_module_ranges(text, attribute_ranges)
    for match in RUST_SERDE_SHAPE.finditer(text):
        if any(start <= match.start() < end for start, end in excluded_ranges):
            continue
        name = match.group(1)
        start = rust_item_start_with_attributes(text, match.start(), attribute_ranges)
        attributes = text[start : match.start()]
        if "Serialize" not in attributes and "Deserialize" not in attributes:
            continue
        end = rust_item_end(text, match.start())
        value = strip_rust_trivia(normalize_rust_derive_lists(text[start:end]))
        key = name
        ordinal = 2
        while key in found:
            key = f"{name}#{ordinal}"
            ordinal += 1
        found[key] = value
    return found


def guard_signature(
    view: RepositoryView,
    revision: str,
    guard: Guard,
    *,
    enforce_presence: bool,
    base_signature: tuple[tuple[str, str], ...] | None = None,
) -> tuple[tuple[str, str], ...]:
    paths = view.matching_paths(revision, guard.paths)
    base_by_key = dict(base_signature) if base_signature is not None else {}
    elide_fn = ELISIONS.get(guard.elide) if guard.elide else None

    def apply_elision(key: str, val: str) -> str:
        if elide_fn is not None and base_signature is not None:
            return elide_fn(val, base_by_key.get(key, ""))
        return val

    signature: list[tuple[str, str]] = []
    found_symbols: set[str] = set()
    covered_shapes: set[str] = set()
    covered_markers: set[str] = set()
    for path in paths:
        content = view.content(revision, path)
        if content is None:
            continue
        if guard.kind == "file":
            signature.append((path, apply_elision(path, content)))
            covered_markers.update(
                marker for marker in guard.must_cover if marker in content
            )
        elif guard.kind == "rust_items":
            items = named_rust_items(content, guard.symbols)
            found_symbols.update(items)
            signature.extend(
                (f"{path}:{name}", apply_elision(f"{path}:{name}", value))
                for name, value in items.items()
            )
        elif guard.kind == "rust_impls":
            items = named_rust_serde_impls(content, guard.symbols)
            found_symbols.update(items)
            signature.extend(
                (f"{path}:{name}", apply_elision(f"{path}:{name}", value))
                for name, value in items.items()
            )
        else:
            items = serde_shapes(content)
            covered_shapes.update(name.partition("#")[0] for name in items)
            signature.extend(
                (f"{path}:{name}", apply_elision(f"{path}:{name}", value))
                for name, value in items.items()
            )

    if guard.kind in {"rust_items", "rust_impls"} and enforce_presence:
        missing = sorted(set(guard.symbols) - found_symbols)
        if missing:
            raise CheckError(
                f"{revision[:12]}: guarded Rust symbols not found in "
                f"{', '.join(guard.paths)}: "
                + ", ".join(missing)
            )
    elif guard.kind == "rust_serde_shapes" and enforce_presence:
        missing = sorted(set(guard.must_cover) - covered_shapes)
        if missing:
            raise CheckError(
                f"{revision[:12]}: required Serde shapes not found in "
                f"{', '.join(guard.paths)}: " + ", ".join(missing)
            )
    elif guard.kind == "file" and enforce_presence:
        if not paths:
            raise CheckError(
                f"{revision[:12]}: guarded file pattern matched nothing: "
                f"{', '.join(guard.paths)}"
            )
        missing = sorted(set(guard.must_cover) - covered_markers)
        if missing:
            raise CheckError(
                f"{revision[:12]}: required file markers not found in "
                f"{', '.join(guard.paths)}: " + ", ".join(missing)
            )
    return tuple(sorted(signature))


def version_at(view: RepositoryView, revision: str, surface: Surface) -> int:
    content = view.content(revision, surface.constant_path)
    if content is None:
        raise CheckError(
            f"{revision[:12]}: cannot read {surface.constant_path} for "
            f"{surface.constant}"
        )
    pattern = surface.version_regex or (
        rf"\b{re.escape(surface.constant)}\b\s*:[^=\n]+="
        r"\s*([0-9][0-9_]*)\s*;"
    )
    try:
        matches = list(re.finditer(pattern, content, re.MULTILINE))
    except re.error as error:
        raise CheckError(
            f"invalid version_regex for {surface.constant}: {error}"
        ) from error
    if len(matches) != 1 or len(matches[0].groups()) != 1:
        raise CheckError(
            f"{revision[:12]}: expected exactly one single-capture version match for "
            f"{surface.constant} in {surface.constant_path}, found {len(matches)}"
        )
    value = matches[0].group(1).replace("_", "")
    try:
        return int(value)
    except ValueError as error:
        raise CheckError(
            f"{revision[:12]}: {surface.constant} version {value!r} is not an integer"
        ) from error


def inventory_keys(document: object) -> frozenset[str] | None:
    """The surface keys a parsed inventory declares, or None if unreadable."""
    if not isinstance(document, dict):
        return None
    raw_surfaces = document.get("surface")
    if not isinstance(raw_surfaces, list):
        return None
    keys: set[str] = set()
    for raw_surface in raw_surfaces:
        if not isinstance(raw_surface, dict):
            return None
        constant = raw_surface.get("constant")
        constant_path = raw_surface.get("constant_path")
        if not isinstance(constant, str) or not isinstance(constant_path, str):
            return None
        keys.add(f"{constant_path}:{constant}")
    return frozenset(keys)


def base_inventory_keys(
    repo: Path, base_revision: str, config: Path
) -> frozenset[str] | None:
    """The surfaces the merge-base inventory declared, or None when undecidable.

    A surface absent from that inventory is registered by this change, so its
    guarded shape has no version to be compared against and none to be bumped
    past.  Whenever the base inventory cannot be read the answer is None and
    every surface is checked as pre-existing, which is the stricter reading.
    """
    try:
        relative = config.resolve().relative_to(repo.resolve()).as_posix()
    except (OSError, ValueError):
        return None
    result = git(repo, "show", f"{base_revision}:{relative}", check=False)
    if result.returncode != 0:
        return None
    try:
        return inventory_keys(tomllib.loads(result.stdout))
    except tomllib.TOMLDecodeError:
        return None


def surface_fingerprint(entries: Iterable[tuple[str, str]]) -> str:
    """Pin the guarded bytes a registration enrolled.

    The digest covers every guarded path and symbol the surface's head
    signature carries, in guard order, so a baseline burned for one enrolment
    cannot be reused by a later change to the same shape.
    """
    digest = hashlib.sha256()
    for key, value in entries:
        digest.update(key.encode("utf-8"))
        digest.update(b"\0")
        digest.update(value.encode("utf-8", errors="surrogateescape"))
        digest.update(b"\0")
    return f"sha256:{digest.hexdigest()}"


def check_surfaces(
    repo: Path,
    base: str,
    head: str,
    surfaces: Iterable[Surface],
    base_keys: frozenset[str] | None = None,
) -> CheckResult:
    base_revision = resolve_revision(repo, base)
    head_revision = resolve_revision(repo, head)
    view = RepositoryView(repo)
    failures: list[Failure] = []
    errors: list[SurfaceError] = []
    registrations: list[Surface] = []
    unregistered: list[Unregistered] = []
    identifier_renames: list[Surface] = []
    stacked_versions: list[Surface] = []
    for surface in surfaces:
        try:
            head_version = version_at(view, head_revision, surface)
        except CheckError as error:
            errors.append(SurfaceError(surface, str(error)))
            continue
        changed_guards: list[str] = []
        head_entries: list[tuple[str, str]] = []
        for guard in surface.guards:
            try:
                base_signature = guard_signature(
                    view, base_revision, guard, enforce_presence=False
                )
                head_signature = guard_signature(
                    view,
                    head_revision,
                    guard,
                    enforce_presence=True,
                    base_signature=base_signature,
                )
            except CheckError as error:
                errors.append(
                    SurfaceError(
                        surface,
                        f"guard {', '.join(guard.paths)}: {error}",
                    )
                )
                continue
            head_entries.extend(head_signature)
            if base_signature != head_signature:
                changed_guards.extend(guard.paths)
        if not changed_guards:
            continue
        try:
            base_version = version_at(view, base_revision, surface)
        except CheckError as error:
            # No merge-base version exists for this constant. Either the change
            # registers the surface -- a burned baseline says so by name and by
            # the exact shape it enrolled -- or the constant was renamed,
            # relocated, or added over a shape that was already moving, and the
            # shape change stands unaccounted for.
            fingerprint = surface_fingerprint(head_entries)
            registered = (
                base_keys is not None
                and surface.key not in base_keys
                and REGISTRATION_BASELINES.get(surface.key) == fingerprint
            )
            if registered:
                registrations.append(surface)
                continue
            if base_keys is None or surface.key in base_keys:
                errors.append(SurfaceError(surface, str(error)))
                continue
            unregistered.append(
                Unregistered(
                    surface=surface,
                    changed_guards=tuple(dict.fromkeys(changed_guards)),
                    fingerprint=fingerprint,
                )
            )
            continue
        if head_version <= base_version:
            # An atomic stack may reserve its one version bump on a lower
            # branch and land the guarded bytes on an upper branch. That
            # exception pins the reviewed final bytes and is not a claim that
            # the wire stayed identical.
            fingerprint = surface_fingerprint(head_entries)
            if STACKED_VERSION_BASELINES.get(surface.key) == fingerprint:
                stacked_versions.append(surface)
                continue
            # The guarded text moved. A burned identifier-rename baseline is a
            # reviewer's signed answer that the format underneath it did not,
            # and it holds only for the exact guarded bytes it pinned.
            fingerprint = surface_fingerprint(head_entries)
            if IDENTIFIER_RENAME_BASELINES.get(surface.key) == fingerprint:
                identifier_renames.append(surface)
                continue
            failures.append(
                Failure(
                    surface=surface,
                    base_version=base_version,
                    head_version=head_version,
                    changed_guards=tuple(dict.fromkeys(changed_guards)),
                    fingerprint=fingerprint,
                )
            )
    return CheckResult(
        tuple(failures),
        tuple(errors),
        tuple(registrations),
        tuple(unregistered),
        tuple(identifier_renames),
        tuple(stacked_versions),
    )


def select_surfaces(
    surfaces: tuple[Surface, ...], selectors: Iterable[str]
) -> tuple[Surface, ...]:
    selected: list[Surface] = []
    for selector in selectors:
        key_matches = [surface for surface in surfaces if surface.key == selector]
        matches = key_matches or [
            surface for surface in surfaces if surface.constant == selector
        ]
        if len(matches) != 1:
            detail = ""
            if matches:
                detail = "; use one of: " + ", ".join(
                    surface.key for surface in matches
                )
            raise CheckError(
                f"--surface {selector} matched {len(matches)} inventory entries"
                f"{detail}"
            )
        selected.extend(matches)
    return tuple(selected)


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base", required=True, help="merge-base commit to compare")
    parser.add_argument("--head", default="HEAD", help="head commit (default: HEAD)")
    parser.add_argument(
        "--config", type=Path, default=DEFAULT_CONFIG, help="surface inventory TOML"
    )
    parser.add_argument(
        "--surface",
        action="append",
        default=[],
        help=(
            "check one surface by unique constant or <constant_path>:<constant> "
            "key (repeatable; diagnostic use)"
        ),
    )
    parser.add_argument("--repo", type=Path, default=ROOT, help=argparse.SUPPRESS)
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv or sys.argv[1:])
    if pre_release(args.repo):
        print(
            "version-bump check paused pre-1.0 under tools/release-mode.toml "
            "(pre_release = true; FIG-3660); set it false at the lash 1.0 cut"
        )
        return 0
    try:
        surfaces = load_config(args.config)
        if args.surface:
            surfaces = select_surfaces(surfaces, args.surface)
        base = resolve_revision(args.repo, args.base)
        head = resolve_revision(args.repo, args.head)
        result = check_surfaces(
            args.repo,
            base,
            head,
            surfaces,
            base_inventory_keys(args.repo, base, args.config),
        )
    except CheckError as error:
        print(f"version-bump check error: {error}", file=sys.stderr)
        return 2

    if result.errors:
        print(
            f"version-bump check errors against merge-base {base[:12]}:",
            file=sys.stderr,
        )
        for error in result.errors:
            print(f"- {error.surface.key}: {error.detail}", file=sys.stderr)

    if result.failures or result.unregistered:
        print(
            f"version-bump check failed against merge-base {base[:12]}:",
            file=sys.stderr,
        )
        for entry in result.unregistered:
            paths = ", ".join(entry.changed_guards)
            print(
                f"- {entry.surface.constant} has no merge-base value in "
                f"{entry.surface.constant_path} and no burned registration "
                f"baseline, but its guarded shape changed ({paths}). A renamed or "
                f"relocated constant keeps its merge-base identity and bumps that; "
                f"a genuinely new surface registers once by adding "
                f"{entry.surface.key!r}: {entry.fingerprint!r} to "
                f"REGISTRATION_BASELINES in scripts/check_version_bumps.py.",
                file=sys.stderr,
            )
        for failure in result.failures:
            paths = ", ".join(failure.changed_guards)
            print(
                f"- {failure.surface.constant} is {failure.head_version}; merge-base "
                f"value is {failure.base_version}. Guarded shape changed ({paths}). Bump "
                f"{failure.surface.constant} strictly past {failure.base_version}. "
                f"If the change only retyped Rust identifiers and a reviewer has "
                f"confirmed the serialized bytes are identical on both sides, burn "
                f"that reading once by adding {failure.surface.key!r}: "
                f"{failure.fingerprint!r} to IDENTIFIER_RENAME_BASELINES in "
                f"scripts/check_version_bumps.py.",
                file=sys.stderr,
            )
    if result.errors:
        return 2
    if result.failures or result.unregistered:
        return 1

    registered = ""
    if result.registrations:
        names = ", ".join(surface.key for surface in result.registrations)
        registered = f"; registered by this change: {names}"
    renamed = ""
    if result.identifier_renames:
        names = ", ".join(surface.key for surface in result.identifier_renames)
        renamed = f"; identifier-rename baseline honoured for: {names}"
    stacked = ""
    if result.stacked_versions:
        names = ", ".join(surface.key for surface in result.stacked_versions)
        stacked = f"; atomic-stack version reservation honoured for: {names}"
    print(
        f"version-bump check passed: {len(surfaces)} surfaces against "
        f"merge-base {base[:12]}{registered}{renamed}{stacked}"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
