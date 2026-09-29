ARG RUNTIME_BASE=ubuntu:24.04
FROM ${RUNTIME_BASE}
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl iproute2 postgresql-client python3 && rm -rf /var/lib/apt/lists/*
COPY target/loadtest-image/bin/ /usr/local/bin/
COPY crates/lash-postgres-store/schema.sql /opt/lash/schema.sql
COPY target/loadtest-image/witness.sql /opt/lash/witness.sql
