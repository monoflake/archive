# syntax=docker/dockerfile:1
# check=skip=InvalidDefaultArgInFrom
# Built from the repository root by `mise run host`. RUST_IMAGE has no default on purpose, which
# is the check skipped above: the task names the rust image of the workspace's toolchain, by digest.
ARG RUST_IMAGE

# sccache, the compiler cache the build runs rustc through: the static binary from its release,
# checked against the sum the release publishes, in a stage for each architecture.
FROM scratch AS sccache-arm64
ADD --checksum=sha256:2b3284d5da3b46a47dc4229e75bb7b88ac4aa99c8d754fb7d2f84997e5a4354a \
	https://github.com/mozilla/sccache/releases/download/v0.18.0/sccache-v0.18.0-aarch64-unknown-linux-musl.tar.gz /sccache.tar.gz
FROM scratch AS sccache-amd64
ADD --checksum=sha256:45f1447fbe231e3037bde351ef70677dd212216c8d62ae7ca409fecc4d6acc89 \
	https://github.com/mozilla/sccache/releases/download/v0.18.0/sccache-v0.18.0-x86_64-unknown-linux-musl.tar.gz /sccache.tar.gz
FROM sccache-${TARGETARCH} AS sccache

# age, which `secrets init` encrypts the root key with to the post-quantum recovery recipient the
# `age` crate does not reach: the static binary from its release, checked against the sum GitHub
# publishes for it, in a stage for each architecture. See src/ceremony.rs.
FROM scratch AS age-arm64
ADD --checksum=sha256:6b8dc4333c53a5a57c9e5834e3a48f92605d7154014cd07269ff3327db5d37f4 \
	https://github.com/FiloSottile/age/releases/download/v1.3.2/age-v1.3.2-linux-arm64.tar.gz /age.tar.gz
FROM scratch AS age-amd64
ADD --checksum=sha256:cbe24006683f8eb669266162894b9a522a1af52f2665fbc63a4bb032ed26ac10 \
	https://github.com/FiloSottile/age/releases/download/v1.3.2/age-v1.3.2-linux-amd64.tar.gz /age.tar.gz
FROM age-${TARGETARCH} AS age

# Static, against musl, as geo is; see its Dockerfile.
FROM ${RUST_IMAGE} AS build
ARG TARGETARCH
# What `__DATE__` and the like read, so the same source builds the same image. See infra's
# spec/architecture/host.md, "An image is built for speed, and for any node of its architecture".
ARG SOURCE_DATE_EPOCH
RUN apt-get update \
	&& apt-get install --yes --no-install-recommends musl-tools \
	&& rm -rf /var/lib/apt/lists/*
RUN --mount=from=sccache,target=/sccache \
	tar -xzf /sccache/sccache.tar.gz -C /usr/local/bin --strip-components=1 --no-same-owner \
		--wildcards '*/sccache'
RUN --mount=from=age,target=/age-release \
	tar -xzf /age-release/age.tar.gz -C / --strip-components=1 --no-same-owner age/age
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY apps apps
COPY libs libs
# CFLAGS compiles mimalloc with transparent huge pages off; see infra's
# spec/architecture/host.md, "An image is built for speed, and for any node of its architecture".
# sccache's backend comes in as the secret `sccache` from .mise/tasks/image, else it is the cache
# mount, and rustc runs alone when its server cannot start; the same section covers sccache.
# rustc reaches it as `rustc-cached`, a name the `cc` crate takes for no C wrapper: a C compile
# through sccache ignores SOURCE_DATE_EPOCH, and mimalloc's C carries its build time.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
	--mount=type=cache,target=/src/target \
	--mount=type=cache,target=/var/cache/sccache,sharing=locked \
	--mount=type=secret,id=sccache \
	case "$TARGETARCH" in arm64) arch=aarch64 ;; amd64) arch=x86_64 ;; *) exit 1 ;; esac \
	&& target="$arch-unknown-linux-musl" \
	&& rustup target add "$target" \
	&& export "CC_${arch}_unknown_linux_musl=musl-gcc" \
	&& export "CFLAGS_${arch}_unknown_linux_musl=-DMI_DEFAULT_ALLOW_THP=0" \
	&& export SCCACHE_DIR=/var/cache/sccache SCCACHE_IDLE_TIMEOUT=0 \
	&& if [ -f /run/secrets/sccache ]; then . /run/secrets/sccache; fi \
	&& if sccache --start-server; then \
		printf '#!/bin/sh\nexec sccache "$@"\n' > /usr/local/bin/rustc-cached \
		&& chmod +x /usr/local/bin/rustc-cached && export RUSTC_WRAPPER=rustc-cached; fi \
	&& cargo build --profile container --locked --target "$target" -p secrets-service \
	&& cp "target/$target/container/secrets" /secrets \
	&& if [ -n "${RUSTC_WRAPPER:-}" ]; then sccache --show-stats || true; fi

FROM scratch
COPY --from=build /secrets /secrets
COPY --from=build /age /age
# Not root: nothing here needs to be, and scratch has no user table, so it is named by number.
USER 65532:65532
ENTRYPOINT ["/secrets"]
