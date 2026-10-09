# Durable transaction telemetry

`lash.durable.commit.lock_statement_elapsed` is a histogram in microseconds
for lock-bearing statements within one physical PostgreSQL transaction attempt,
labelled by commit and outcome. It includes execution and network time and is
an upper bound on lock wait. It does not isolate server lock-wait time; its
existing producer and name already preserve this distinction.
