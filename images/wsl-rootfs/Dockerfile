# Rusker WSL2 system image, the counterpart of Docker Desktop's
# `docker-desktop` system image: a minimal glibc userland (Debian 12, like
# Docker Desktop) with the official Docker Engine, Compose/Buildx and the
# NVIDIA Container Toolkit preinstalled. The app downloads one pinned,
# checksummed tarball and imports it with `wsl --import`; nothing is
# installed from the network on the user's machine.
#
# Images, containers and volumes never live here: they are on the separate
# data disk (`docker-data.vhdx`), so this system disk can be replaced on
# upgrade or repair.
#
# Build with `build.sh`; see docs/wsl2-rootfs.md.

ARG BASE_IMAGE=debian:12-slim
FROM ${BASE_IMAGE}

ARG IMAGE_VERSION
ARG DOCKER_VERSION=29.9.2
ARG CONTAINERD_VERSION=2.4.2
ARG COMPOSE_VERSION=5.6.1
ARG BUILDX_VERSION=0.38.1
ARG NVIDIA_TOOLKIT_VERSION=1.20.1-1

COPY install.sh /tmp/rusker-install.sh
RUN IMAGE_VERSION="${IMAGE_VERSION:?IMAGE_VERSION is required}" \
    DOCKER_VERSION="${DOCKER_VERSION}" \
    CONTAINERD_VERSION="${CONTAINERD_VERSION}" \
    COMPOSE_VERSION="${COMPOSE_VERSION}" \
    BUILDX_VERSION="${BUILDX_VERSION}" \
    NVIDIA_TOOLKIT_VERSION="${NVIDIA_TOOLKIT_VERSION}" \
    sh /tmp/rusker-install.sh \
    && rm -f /tmp/rusker-install.sh
