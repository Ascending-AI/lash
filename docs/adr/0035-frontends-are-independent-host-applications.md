# Frontends are independent host applications

## Context

A user-facing frontend has UI and distribution policy that need not force a
runtime release. It also exercises Lash as an external embedder.

## Decision

Terminal and other user-facing frontends live in their own repositories. Each
host owns its UI, extensions, file index, transcript exporter, applied research
integrations, benchmark support, installer, update policy and frontend binary
releases.

Lash owns reusable runtime, protocol, provider, persistence, plugin, tooling
and performance contracts. A frontend consumes those contracts at a reviewed,
exact Lash revision and advances that revision through an explicit
compatibility change. ADR 0079 governs the promised package API.

Lash publishes SDK crates and the `lashctl` operator binary under ADR 0115.
The operator binary owns Lash's part of upgrades; it does not move frontend
ownership or host product policy into this repository. Examples exercise the
embedding contracts.

A support crate stays with its host while that host is its only real consumer.
It belongs in Lash when it is a stable frontend-independent contract with
credible use by another host.

## Consequences

Hosts choose plugin composition and Execution Modes independently of runtime
releases. Runtime changes must work through public package boundaries without
private workspace paths. Folding a frontend's private support code into Lash
is rejected while its only consumer is that frontend.

## Implementation

The [workspace manifest](../../Cargo.toml), [facade package](../../crates/lash/Cargo.toml)
and [operator package](../../crates/lashctl/Cargo.toml) define the shipped
repository boundary.
