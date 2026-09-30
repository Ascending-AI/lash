ARG RUNTIME_BASE=ubuntu:24.04
FROM ${RUNTIME_BASE}
# The binaries of one worker generation: the bootstrap build, or the
# synthetic N+1 the rolling deploy starts (FIG-4169).
ARG BIN_DIR=target/loadtest-image/bin
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl iproute2 postgresql-client python3 && rm -rf /var/lib/apt/lists/*
COPY ${BIN_DIR}/ /usr/local/bin/
COPY crates/lash-postgres-store/schema.sql /opt/lash/schema.sql
COPY target/loadtest-image/witness.sql /opt/lash/witness.sql
