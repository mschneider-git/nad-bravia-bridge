# The binary is cross-compiled on the build host (no emulation needed) into a
# fully static musl executable, so the runtime image needs nothing else.
FROM --platform=$BUILDPLATFORM rust:1-slim AS build
ARG TARGETARCH
ENV CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld \
    CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld
WORKDIR /src
RUN case "$TARGETARCH" in \
        amd64) echo x86_64-unknown-linux-musl ;; \
        arm64) echo aarch64-unknown-linux-musl ;; \
        *) echo "unsupported arch $TARGETARCH" >&2; exit 1 ;; \
    esac > /rust-target \
    && rustup target add "$(cat /rust-target)"
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked --target "$(cat /rust-target)" \
    && cp "target/$(cat /rust-target)/release/nadbridge" /nadbridge

FROM scratch
ARG BUILD_VERSION
ARG BUILD_ARCH
LABEL io.hass.type="addon" \
      io.hass.version="${BUILD_VERSION}" \
      io.hass.arch="${BUILD_ARCH}"
# Home Assistant mounts the host's D-Bus at /run/dbus (host_dbus: true).
ENV DBUS_SYSTEM_BUS_ADDRESS=unix:path=/run/dbus/system_bus_socket
COPY --from=build /nadbridge /nadbridge
ENTRYPOINT ["/nadbridge"]
