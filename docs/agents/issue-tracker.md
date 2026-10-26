# Issue tracker

Lash work items live in Linear, team `figments`, project `lash`, with keys
`FIG-<n>`. Refer to an issue by its linked title. Use the installed `linear`
CLI or available Linear tools. Run `linear issue <command> --help` for exact
options; agents without tracker access need the brief supplied in their task.

```sh
linear issue view FIG-<n>
linear issue comment add FIG-<n> --body-file <evidence-file>
linear issue update FIG-<n> --state Done
```

A multi-ticket effort has one orienting parent and related child issues.
Native parent and blocking relations carry hierarchy and dependencies; inspect
those relations before claiming a child. Claiming assigns the issue and moves
it to In Progress. States and dispatch labels are defined in
[way-of-working.md](way-of-working.md#lifecycle-states-and-labels).

[ticket-style.md](ticket-style.md) defines the ticket and Agent spec format.
Put research and investigation results in a comment on the requesting ticket.
Echo an accepted decision into the issue body and parent index before closing,
and promote lasting architectural decisions to an ADR.
