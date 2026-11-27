FROM rust:1.95-trixie AS builder
WORKDIR /build
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
COPY Cargo.toml Cargo.lock ./
COPY crates/memento-core/Cargo.toml crates/memento-core/Cargo.toml
COPY crates/mementod/Cargo.toml crates/mementod/Cargo.toml
COPY crates/memento-cli/Cargo.toml crates/memento-cli/Cargo.toml
COPY crates/memento-mcp/Cargo.toml crates/memento-mcp/Cargo.toml
RUN mkdir -p crates/memento-core/src crates/mementod/src crates/memento-cli/src crates/memento-mcp/src && printf '' > crates/memento-core/src/lib.rs && printf '' > crates/mementod/src/lib.rs && printf 'fn main() {}\n' > crates/mementod/src/main.rs && printf 'fn main() {}\n' > crates/memento-cli/src/main.rs && printf 'fn main() {}\n' > crates/memento-mcp/src/main.rs && cargo build --locked --release -p mementod && rm -rf crates
COPY crates/memento-core crates/memento-core
COPY crates/mementod crates/mementod
COPY crates/memento-cli crates/memento-cli
COPY crates/memento-mcp crates/memento-mcp
RUN touch crates/memento-core/src/lib.rs crates/mementod/src/lib.rs crates/mementod/src/main.rs && cargo build --locked --release -p mementod

FROM debian:trixie-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/* && useradd --create-home --uid 10001 memento && mkdir -p /opt/memento/models && chown -R memento:memento /opt/memento
COPY --from=builder /build/target/release/mementod /usr/local/bin/mementod
ENV FASTEMBED_CACHE_DIR=/opt/memento/models
ENV MEMENTO_BIND=0.0.0.0:7377
USER memento
RUN mementod warmup
ENTRYPOINT ["mementod"]
CMD ["serve"]
