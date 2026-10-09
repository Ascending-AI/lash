# Load models

Every receipt names how its operations were generated in `load_model`.

| `load_model` | Arrivals | Latency clock starts | Instruments |
|---|---|---|---|
| `offered-load` | Independent, on a schedule | At the scheduled arrival | `lash-perf offered-load` |
| `service-diagnostic` | Closed loop: the next operation is sent when the last one returns | At the send | Every runtime-perf scenario (including `high_traffic_load_sqlite` and `high_traffic_knee_sqlite`), every `lash-perf latency` case, and the PostgreSQL live-replay bench |

A closed loop delays its own arrivals behind a slow operation and never
records their wait, so its tail and its knee understate queueing. Use a
`service-diagnostic` receipt to compare service time at a fixed concurrency.
Take tails, capacity and the saturation knee from an `offered-load` receipt.
`lash-perf offered-load` holds the only arrival generator.
