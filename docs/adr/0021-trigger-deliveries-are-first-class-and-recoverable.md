# Trigger deliveries are first-class and recoverable

## Status

Replaced by [ADR 0136](0136-the-host-owns-events-routing-and-scheduling.md).

## Decision

The host owns trigger registrations, source provisioning, occurrence records,
routing and scheduling. It commits its product record before delivering
through keyed process starts, sends or completion resolution. ADR 0136 owns
the delivery and retention contract and the host patterns.
