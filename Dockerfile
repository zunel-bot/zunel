FROM rust:1.95-bookworm AS builder

WORKDIR /app

COPY rust/Cargo.toml rust/Cargo.lock ./rust/
COPY rust/crates/ ./rust/crates/

RUN cargo build --manifest-path rust/Cargo.toml --release -p zunel-cli --locked

FROM debian:bookworm-slim

RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates git bubblewrap openssh-client && \
    rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY --from=builder /app/rust/target/release/zunel /usr/local/bin/zunel

# Create non-root user and config directory.
RUN useradd -m -u 1000 -s /bin/bash zunel && \
    mkdir -p /home/zunel/.zunel && \
    chown -R zunel:zunel /home/zunel /app

COPY entrypoint.sh /usr/local/bin/entrypoint.sh
RUN sed -i 's/\r$//' /usr/local/bin/entrypoint.sh && chmod +x /usr/local/bin/entrypoint.sh

USER zunel
ENV HOME=/home/zunel

ENTRYPOINT ["entrypoint.sh"]
CMD ["status"]
