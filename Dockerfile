# Production image for a Kubernetes-native reconcile node.
#
# Multi-stage build: compile the `k8s_node` example with the Prometheus metrics endpoint, then ship
# only the binary on a distroless glibc base. glibc (not musl) is deliberate: peer discovery resolves
# the headless Service through `getaddrinfo` (`tokio::net::lookup_host`), which is most reliable on
# glibc. A static musl image would need the (reserved) `dns-hickory` feature instead.
#
# Build:  docker build -t reconcile:latest .
# The container reads its configuration from the environment — see deploy/k8s/.

# ---- builder ----
FROM rust:1-bookworm AS builder
WORKDIR /src

# Copy the manifest and sources. (Cargo.lock is copied if present; ignored otherwise.)
COPY Cargo.toml Cargo.lock* ./
COPY src ./src
COPY examples ./examples
COPY benches ./benches

# Build the Kubernetes node binary with the metrics/readiness endpoint compiled in.
RUN cargo build --release --example k8s_node --features metrics-prometheus

# ---- runtime ----
FROM gcr.io/distroless/cc-debian12 AS runtime
COPY --from=builder /src/target/release/examples/k8s_node /usr/local/bin/k8s_node

# Gossip (UDP) and metrics/probes (TCP). Match deploy/k8s/ and the container env.
EXPOSE 8080/udp
EXPOSE 9000/tcp

# Run as the distroless non-root user.
USER 65532:65532

ENTRYPOINT ["/usr/local/bin/k8s_node"]
