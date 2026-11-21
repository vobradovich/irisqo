# syntax=docker/dockerfile:1
FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
# On a small builder (a 2 GiB VM) limit the parallel rustc jobs: --build-arg CARGO_BUILD_JOBS=2
ARG CARGO_BUILD_JOBS
RUN cargo build --release --locked

FROM debian:bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --no-create-home irisqo
COPY --from=build /src/target/release/irisqo /usr/local/bin/irisqo
USER irisqo
EXPOSE 8102
ENTRYPOINT ["irisqo"]
