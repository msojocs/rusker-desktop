#!/bin/sh
# Runs inside the image build. Installs pinned Docker Engine and NVIDIA
# Container Toolkit packages from their official signed APT repositories,
# writes the Rusker configuration and strips everything not needed at run
# time.
set -eu
export DEBIAN_FRONTEND=noninteractive

apt-get update
apt-get install -y --no-install-recommends ca-certificates curl gnupg

install -m 0755 -d /etc/apt/keyrings
arch=$(dpkg --print-architecture)
. /etc/os-release

curl -fsSL https://download.docker.com/linux/debian/gpg \
    | gpg --batch --yes --dearmor -o /etc/apt/keyrings/docker.gpg
printf 'deb [arch=%s signed-by=/etc/apt/keyrings/docker.gpg] https://download.docker.com/linux/debian %s stable\n' \
    "$arch" "$VERSION_CODENAME" > /etc/apt/sources.list.d/docker.list

curl -fsSL https://nvidia.github.io/libnvidia-container/gpgkey \
    | gpg --batch --yes --dearmor -o /etc/apt/keyrings/nvidia-container-toolkit.gpg
curl -fsSL https://nvidia.github.io/libnvidia-container/stable/deb/nvidia-container-toolkit.list \
    | sed 's#deb https://#deb [signed-by=/etc/apt/keyrings/nvidia-container-toolkit.gpg] https://#g' \
    > /etc/apt/sources.list.d/nvidia-container-toolkit.list

apt-get update

# Resolves an upstream version (e.g. 29.9.2) to the exact package version in
# the repository (e.g. 5:29.9.2-1~debian.12~bookworm); fails if absent.
pick() {
    found=$(apt-cache madison "$1" | awk -v want="$2" '{
        version = $3; plain = version; sub(/^[0-9]+:/, "", plain)
        if (plain == want || index(plain, want "-") == 1) { print version; exit }
    }')
    if [ -z "$found" ]; then
        echo "package $1 version $2 not found; available:" >&2
        apt-cache madison "$1" | head -n 10 >&2
        exit 1
    fi
    printf '%s' "$found"
}

docker_ce=$(pick docker-ce "$DOCKER_VERSION")
docker_cli=$(pick docker-ce-cli "$DOCKER_VERSION")
containerd=$(pick containerd.io "$CONTAINERD_VERSION")
compose=$(pick docker-compose-plugin "$COMPOSE_VERSION")
buildx=$(pick docker-buildx-plugin "$BUILDX_VERSION")

# Only what the managed engine and the app's scripts use: iptables (nft, as
# in Docker Desktop) for container networking, e2fsprogs/util-linux for the
# data disk, iproute2 for LAN forwarding, xz/pigz for image layers. No Linux
# display driver: WSL2 maps the Windows NVIDIA driver into /usr/lib/wsl.
apt-get install -y --no-install-recommends \
    "docker-ce=$docker_ce" \
    "docker-ce-cli=$docker_cli" \
    "containerd.io=$containerd" \
    "docker-compose-plugin=$compose" \
    "docker-buildx-plugin=$buildx" \
    "nvidia-container-toolkit=$NVIDIA_TOOLKIT_VERSION" \
    "nvidia-container-toolkit-base=$NVIDIA_TOOLKIT_VERSION" \
    "libnvidia-container-tools=$NVIDIA_TOOLKIT_VERSION" \
    "libnvidia-container1=$NVIDIA_TOOLKIT_VERSION" \
    iptables e2fsprogs util-linux iproute2 procps xz-utils pigz

# Register the `nvidia` runtime (`--gpus all`), as Docker Desktop does.
mkdir -p /etc/docker
nvidia-ctk runtime configure --runtime=docker --config=/etc/docker/daemon.json

# The image is replaced as a whole on upgrade, like Docker Desktop's; pinned
# third-party repositories are removed so nothing drifts in place.
apt-get purge -y --auto-remove curl gnupg
rm -f /etc/apt/sources.list.d/docker.list /etc/apt/sources.list.d/nvidia-container-toolkit.list \
    /etc/apt/keyrings/docker.gpg /etc/apt/keyrings/nvidia-container-toolkit.gpg

printf 'rusker-wsl2-v1\n' > /etc/rusker-managed
printf 'rusker\n' > /etc/hostname
cat > /etc/wsl.conf <<'EOF'
[boot]
systemd=false

[network]
hostname = rusker

[user]
default = root
EOF

cat > /etc/rusker-image <<EOF
RUSKER_IMAGE=rusker-rootfs-v$IMAGE_VERSION
BASE=$PRETTY_NAME
DOCKER=$docker_ce
CONTAINERD=$containerd
COMPOSE=$compose
BUILDX=$buildx
NVIDIA_CONTAINER_TOOLKIT=$NVIDIA_TOOLKIT_VERSION
EOF

# Strip caches, docs and locales (Docker Desktop's image does the same).
apt-get clean
rm -rf /var/lib/apt/lists/* /var/cache/apt/* /var/cache/debconf/*-old \
    /var/lib/dpkg/*-old /var/log/* /tmp/* /var/tmp/* \
    /usr/share/doc/* /usr/share/man/* /usr/share/info/* /usr/share/lintian \
    /usr/share/locale/* /usr/share/bash-completion/completions/docker \
    /usr/lib/systemd/system/docker.* /usr/lib/systemd/system/containerd.service \
    /etc/init.d/docker /etc/default/docker
# Docker's own layer store lives on the data disk, never in the image;
# `ctr` is a containerd debugging client that dockerd does not use.
rm -rf /var/lib/docker /var/lib/containerd /usr/bin/ctr

cat /etc/rusker-image
