# Production image for a Kubernetes-native reconcile node.
#
# Multi-stage build: compile a reconcile example with the Prometheus metrics endpoint, then ship
# only the binary on a distroless glibc base. glibc (not musl) is deliberate: peer discovery resolves
# the headless Service through `getaddrinfo` (`tokio::net::lookup_host`), which is most reliable on
# glibc. A static musl image would need the (reserved) `dns-hickory` feature instead.
#
# The build context is the repository root (where src/ and examples/ live), so build from there
# with an explicit -f:
#   docker build -f kubernetes/Dockerfile -t reconcile:latest .
# The container reads its configuration from the environment — see kubernetes/base/.
#
# Which example to compile is selectable via the EXAMPLE build arg (default `k8s_node`, the
# production node). The local kind playground builds `k8s_heartbeat`, which additionally writes a
# per-pod heartbeat key and logs reconciled keys so convergence is visible in the logs:
#   docker build -f kubernetes/Dockerfile --build-arg EXAMPLE=k8s_heartbeat -t reconcile:kind .

# Selectable at build time; redeclared inside each stage that uses it (Docker ARG scoping).
ARG EXAMPLE=k8s_node

# ---- builder ----
FROM rust:1-bookworm AS builder
ARG EXAMPLE
WORKDIR /src

# Copy the manifest and sources. (Cargo.lock is copied if present; ignored otherwise.)
COPY Cargo.toml Cargo.lock* ./
COPY src ./src
COPY examples ./examples
COPY benches ./benches

# Build the selected example with the metrics/readiness endpoint compiled in, then move it to a
# fixed path so the runtime stage and ENTRYPOINT don't depend on the example name.
RUN cargo build --release --example "${EXAMPLE}" --features metrics-prometheus \
    && cp "target/release/examples/${EXAMPLE}" /usr/local/bin/reconcile-node

# ---- runtime ----
FROM gcr.io/distroless/cc-debian12 AS runtime
COPY --from=builder /usr/local/bin/reconcile-node /usr/local/bin/reconcile-node

# Gossip (UDP) and metrics/probes (TCP). Match kubernetes/base/ (and kubernetes/kind/) and the env.
EXPOSE 8080/udp
EXPOSE 9000/tcp

# Run as the distroless non-root user.
USER 65532:65532

ENTRYPOINT ["/usr/local/bin/reconcile-node"]
