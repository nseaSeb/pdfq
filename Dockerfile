# syntax=docker/dockerfile:1.7
# PDFQ image: `pdfq` (server) and `pdfq-worker` on distroless, non-root.
# Base images are pinned by multi-arch index digest; bump both together with the toolchain.
ARG RUST_IMAGE=rust:1.99.1-bookworm@sha256:e6f2cff35d0f309acf8433b4f05878ed77092602c5065cc81801177da0bb3a9c
ARG RUNTIME_IMAGE=gcr.io/distroless/cc-debian12:nonroot@sha256:9dac0a79194e45a7da0158a9c6da57b217585af0786db3845d1f0ec1a0dd182f

FROM ${RUST_IMAGE} AS build
ARG TARGETARCH
# Use the image's toolchain, not a download driven by rust-toolchain.toml.
ENV RUSTUP_TOOLCHAIN=1.99.1
WORKDIR /src
COPY . .
# Cache mounts never reach the image layer: copy the binaries out in the same step.
RUN --mount=type=cache,id=pdfq-cargo-${TARGETARCH},target=/usr/local/cargo/registry \
    --mount=type=cache,id=pdfq-target-${TARGETARCH},target=/src/target \
    cargo --version \
 && cargo build --locked --release -p pdfq-server -p pdfq-worker -p pdfq-cli \
 && mkdir -p /out \
 && cp target/release/pdfq target/release/pdfq-worker target/release/pdfq-cli /out/

FROM ${RUST_IMAGE} AS packages
# Override the preloaded set with `--build-arg PACKAGES=path/in/context.txt`
# (an empty list is allowed; adjust the smoke document accordingly).
ARG PACKAGES=packages/packages.txt
ARG SMOKE=packages/smoke.typ
COPY scripts/fetch-packages.sh /usr/local/bin/
COPY ${PACKAGES} /tmp/packages.txt
COPY ${SMOKE} /tmp/smoke.typ
RUN fetch-packages.sh /opt/pdfq/packages /tmp/packages.txt /tmp/smoke.typ
COPY --from=build /out/pdfq-cli /usr/local/bin/
# The preloaded packages must compile with the pinned Typst version.
RUN pdfq-cli compile /tmp/smoke.typ --package-path /opt/pdfq/packages -o /tmp/smoke.pdf

FROM ${RUNTIME_IMAGE}
# Execute-only for nonroot (owned by root): exec of a binary the user cannot read
# starts non-dumpable, so a sibling worker never gets a window to open /proc/<pid>/mem.
COPY --chmod=0711 --from=build /out/pdfq /out/pdfq-worker /usr/local/bin/
COPY --from=packages /opt/pdfq/packages /opt/pdfq/packages
ENV PDFQ_BIND=0.0.0.0:3000 \
    PDFQ_FONT_PATHS=/fonts \
    PDFQ_PACKAGE_PATHS=/opt/pdfq/packages,/packages \
    PDFQ_TEMPLATE_DIR=/templates
EXPOSE 3000
USER nonroot
# /health is 503 until a worker is up (fonts preloaded), and whenever none can start.
HEALTHCHECK --interval=30s --timeout=5s --start-period=30s --start-interval=1s --retries=3 \
    CMD ["/usr/local/bin/pdfq", "healthcheck"]
ENTRYPOINT ["/usr/local/bin/pdfq"]
