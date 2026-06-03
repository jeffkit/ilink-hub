# Multi-stage build for minimal final image
FROM rust:1.87-slim AS builder

WORKDIR /app

# Cache dependency layers
COPY Cargo.toml ./
RUN mkdir src && echo "fn main() {}" > src/main.rs
RUN cargo build --release --locked || true
RUN rm -rf src

# Build actual code
COPY src ./src
RUN touch src/main.rs && cargo build --release --locked

FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/ilink-hub /usr/local/bin/ilink-hub

ENV ILINK_HUB_ADDR=0.0.0.0:8765
ENV DATABASE_URL=sqlite:/data/ilink-hub.db

VOLUME ["/data"]
EXPOSE 8765

ENTRYPOINT ["ilink-hub"]
CMD ["serve"]
