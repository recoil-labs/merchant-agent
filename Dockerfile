# syntax=docker/dockerfile:1.7
#
# The RecoilPay Merchant Agent image.
#
#   docker build -t merchant-agent --build-arg GIT_COMMIT=$(git rev-parse --short=12 HEAD) merchant-agent/
#
# Final image: distroless (glibc, CA certificates, nothing else: no shell,
# no package manager), running as UID 10001. The agent writes only to
# /var/lib/merchant-agent, so the root filesystem can be read-only. The
# config is mounted at /etc/merchant-agent/merchant-agent.yaml.
#
# Base images are pinned by digest; bump them on purpose.

FROM rust:1.94.1-slim-bookworm@sha256:cf9dd0ec73e75f827fe59123fff9dc65af1a1c8363c3c31ee8d7f8ad0b6a5fb2 AS build
ARG GIT_COMMIT=unknown
# Secret backends compiled in. File and env are always there; gcp and aws
# add the cloud secret managers.
ARG FEATURES="gcp aws"
# The image's own toolchain, the version rust-toolchain.toml pins. Set here so
# rustup doesn't go online for that file's rustfmt and clippy, which a release
# build doesn't use.
ENV RUSTUP_TOOLCHAIN=1.94.1
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/src/target,sharing=locked \
    MERCHANT_AGENT_GIT_COMMIT="$GIT_COMMIT" \
    cargo build --release --locked -p merchant-agent --features "$FEATURES" \
 && install -D -m 0755 target/release/merchant-agent /out/merchant-agent \
 && install -d -m 0700 /out/data

FROM gcr.io/distroless/cc-debian12:nonroot@sha256:9dac0a79194e45a7da0158a9c6da57b217585af0786db3845d1f0ec1a0dd182f
LABEL org.opencontainers.image.title="RecoilPay Merchant Agent" \
      org.opencontainers.image.description="Pays RecoilPay cash-outs from the merchant's own bank rail" \
      org.opencontainers.image.source="https://github.com/recoil-labs/merchant-agent" \
      org.opencontainers.image.licenses="LicenseRef-RecoilPay-Merchant-Agent"
COPY --from=build /out/merchant-agent /usr/local/bin/merchant-agent
COPY --from=build /src/LICENSE /usr/share/doc/merchant-agent/LICENSE
# The data directory, owned by the agent's user, so a fresh named volume
# mounted here starts with the right owner.
COPY --from=build --chown=10001:10001 /out/data /var/lib/merchant-agent
USER 10001:10001
ENV MERCHANT_AGENT_CONFIG=/etc/merchant-agent/merchant-agent.yaml
VOLUME ["/var/lib/merchant-agent"]
# The console (8740) and the health port (8741).
EXPOSE 8740 8741
HEALTHCHECK --interval=30s --timeout=5s --start-period=60s --retries=3 \
  CMD ["/usr/local/bin/merchant-agent", "healthcheck"]
ENTRYPOINT ["/usr/local/bin/merchant-agent"]
CMD ["run"]
