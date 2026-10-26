FROM postgres:18-alpine@sha256:77f585114c32fbca283dc835b0596f4e52b51b4c6662d7810b2f4084f60a1873 AS build
RUN apk add --no-cache build-base curl linux-headers python3
WORKDIR /build
RUN curl -fsSL https://codeload.github.com/postgrespro/pg_wait_sampling/tar.gz/adc56074db80b476c9928b6f324b81cb1e89035b -o source.tar.gz \
 && echo '0cfed673a7e1b57ca93253f91b749d9919b8bf2812cda4d9f6c36348f4bf90c4  source.tar.gz' | sha256sum -c - \
 && tar xzf source.tar.gz --strip-components=1
COPY instrument.py /build/instrument.py
RUN python3 --version
RUN python3 instrument.py && make USE_PGXS=1 with_llvm=no && make USE_PGXS=1 with_llvm=no install
FROM postgres:18-alpine@sha256:77f585114c32fbca283dc835b0596f4e52b51b4c6662d7810b2f4084f60a1873
COPY --from=build /usr/local/lib/postgresql/pg_wait_sampling.so /usr/local/lib/postgresql/
COPY --from=build /usr/local/share/postgresql/extension/pg_wait_sampling* /usr/local/share/postgresql/extension/
