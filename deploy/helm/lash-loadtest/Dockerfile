ARG RUNTIME_BASE=ubuntu:24.04
ARG RESTATE_IMAGE
FROM ${RESTATE_IMAGE} AS restate-tools
FROM ${RUNTIME_BASE}
# The binaries of one worker generation: the bootstrap build, or the
# synthetic N+1 the rolling deploy starts (FIG-4169).
ARG BIN_DIR=target/loadtest-image/bin
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl iproute2 postgresql-client python3 && rm -rf /var/lib/apt/lists/*
COPY ${BIN_DIR}/ /usr/local/bin/
# The worker runs model code only in the VM helper beside it (FIG-4161).
RUN test -x /usr/local/bin/lash-e2e-worker && test -x /usr/local/bin/lash-vm-worker
COPY crates/lash-postgres-store/schema.sql /opt/lash/schema.sql
COPY target/loadtest-image/witness.sql /opt/lash/witness.sql
COPY scripts/loadtest_resources.py /opt/lash/loadtest_resources.py
COPY scripts/loadtest_storage.py /opt/lash/loadtest_storage.py
COPY --from=restate-tools /usr/local/bin/restatectl /usr/local/bin/restatectl
