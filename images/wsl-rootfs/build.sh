#!/bin/sh
# Builds the Rusker WSL2 system image tarballs with Docker Buildx:
#   images/wsl-rootfs/build.sh            # version from VERSION, amd64 + arm64
#   ARCHES=amd64 OUT=/tmp/rootfs images/wsl-rootfs/build.sh
# Writes rusker-rootfs-v<version>-<arch>.tar.xz and SHA256SUMS to $OUT.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
IMAGE_VERSION=${IMAGE_VERSION:-$(tr -d ' \r\n' < "$here/VERSION")}
ARCHES=${ARCHES:-"amd64 arm64"}
OUT=${OUT:-dist/rootfs}
mkdir -p "$OUT"
for arch in $ARCHES; do
    name="rusker-rootfs-v${IMAGE_VERSION}-${arch}.tar.xz"
    tarball="$OUT/rootfs-${arch}.tar"
    echo "==> building $name"
    rm -f "$tarball"
    docker buildx build \
        --platform "linux/${arch}" \
        --build-arg "IMAGE_VERSION=${IMAGE_VERSION}" \
        --output "type=tar,dest=${tarball}" \
        "$here"
    xz -T0 -9 -c "$tarball" > "$OUT/$name.partial"
    rm -f "$tarball"
    mv -f "$OUT/$name.partial" "$OUT/$name"
done
(cd "$OUT" && sha256sum rusker-rootfs-v"${IMAGE_VERSION}"-*.tar.xz > SHA256SUMS && cat SHA256SUMS)
