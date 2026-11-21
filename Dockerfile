# syntax=docker/dockerfile:1

# Build stage: the toolchain pinned by rust-toolchain.toml, on Debian bookworm.
FROM rust:1.100-slim-bookworm AS build
WORKDIR /src
COPY . .
# The channel rust-toolchain.toml pins, named through RUSTUP_TOOLCHAIN so that rustup skips that file's rustfmt, clippy
# and llvm-tools components, which building does not need and it would otherwise download.
RUN toolchain="$(sed -n 's/^channel = "\(.*\)"$/\1/p' rust-toolchain.toml)" \
    && test -n "$toolchain" \
    && RUSTUP_TOOLCHAIN="$toolchain" cargo build --release --locked --bin zenodotus

# Runtime stage: glibc and CA certificates only, no shell, running as an unprivileged user.
FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /src/target/release/zenodotus /usr/local/bin/zenodotus
# The config file and the index live in /data; mount a volume there and point data_dir at it.
ENV ZENODOTUS_CONFIG=/data/zenodotus.toml
WORKDIR /data
VOLUME ["/data"]
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/zenodotus"]
CMD ["serve"]
