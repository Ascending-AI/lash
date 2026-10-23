# Lash VM workflows use Draft authoring and standalone source

## Status

Accepted.

## Context

Hosts visualize, edit and publish kernel workflow Documents. Installed TypeScript and Python dialects provide source views; hosts own presentation and layout.

## Decision

The kernel Document is the authoring representation. Workflow graph views and type facets are derived read models. Hosts apply typed authoring::Operation transactions to a Draft; atomic admission and correspondence describe each applied batch. A graph projection is not a second mutable program and does not require an editing inverse.

Opening a saved callable materializes its frozen value graph as ordinary declarations inside a closed entry. Captured values and names can be changed like other document content. Taking the Draft's Document and publishing it creates an independent Linked definition under the environment the host chooses. The new definition does not name its source definition; existing runs retain their original code.

Printing produces standalone source for the representations the selected dialect can express. The host edits text freely and uses ordinary source import and publication, without a base definition or text-edit token. Successful unchanged print/import/publication preserves the public contract, values, sharing and observable behavior under equivalent dependency bindings. It need not preserve definition identity, original formatting, private names, internal Sites or metering. Unsupported representations refuse with a typed reason. Supported captions survive through mapped sites; unspellable captions are reported.

A Draft has a content identity but no published runtime authority. Typed edit correspondence begins at the document opened in that Draft. Runtime overlays name the document actually executed; hosts own versions, revision checks, layout and any historical relationship between definitions. Source replacement creates a new document and does not invent edit history.

Calls, effects, terminal values, data expressions, updates and computations are
typed nodes. Control flow and process bodies have nested subgraphs. Sequencing
preserves effect order; a composite expression retains short-circuit and unwrap
behavior without scheduling its operands independently. Binding resolution
preserves lexical shadowing and writes. Diagnostics and argument slots use typed
paths; derived facets select no execution semantics (ADR 0100).

## Consequences

Code and graph views share one kernel Document. Projection does not execute code.
Hosts own revision history, layout and retention. Source compilation checks fresh
policy and dependency choices. Editing leaves parked executions on their recorded code.

## Implementation

- `crates/lash-kernel-edit/src/draft.rs` and `authoring.rs`.
- `crates/lash-kernel-package/src/materialize.rs`.
- `crates/lash-kernel-view` and `crates/lash-kernel-dialect/src/regions.rs`.
- `crates/lash-dialect-typescript/src/readable` and `crates/lash-dialect-python/src/readable.rs`.
