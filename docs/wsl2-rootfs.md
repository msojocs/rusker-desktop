# WSL2 系统镜像

`rusker` 专用发行版的系统盘由 Rusker 自己构建的精简镜像导入，作用相当于 Docker Desktop 的 `docker-desktop.iso`。和 Docker Desktop 一样，镜像**随 Windows 安装包分发**，安装在 `<安装目录>\resources\wsl\`，应用不会联网下载它。构建源码在 [`images/wsl-rootfs`](../images/wsl-rootfs)。

## 与 Docker Desktop 的对照

Docker Desktop 一列取自本地安装（Engine 29.9.2）运行中的 `docker-desktop` 发行版。

| | Docker Desktop | Rusker 系统镜像 v1 |
| --- | --- | --- |
| 基础 | 扁平化的 Debian 12 glibc 用户态（LinuxKit 引导） | Debian 12 slim |
| Engine | dockerd 29.9.2 / containerd 2.4.2 / runc 1.5.2 | Docker 官方 APT 仓库：docker-ce 29.9.2、containerd.io 2.4.2、Compose 5.6.1、Buildx 0.38.1 |
| GPU | NVIDIA Container Toolkit 1.20.1，daemon.json 中注册 `nvidia` runtime | NVIDIA 官方 APT 仓库的 NVIDIA Container Toolkit 1.20.1-1，`nvidia-ctk runtime configure` 注册 `nvidia` runtime |
| 防火墙 | iptables 1.8.9（nf_tables） | iptables 1.8.9（nf_tables） |
| 交付 | 安装包内 `resources\docker-desktop.iso` | 安装包内 `resources\wsl\rusker-rootfs-v<N>-amd64.tar.xz`，`wsl --import` 导入 |
| 数据 | `docker_data.vhdx` | `docker-data.vhdx`（不在镜像中） |
| 升级 | 随应用更新替换系统镜像 | 应用要求新的镜像版本时，“修复环境”用安装包里的新镜像替换系统盘 |

压缩后约 98 MB（解压约 470 MB），安装包相应变大。体积绝大部分是 Docker 官方的静态 Go 程序（dockerd、docker、containerd、Compose/Buildx 插件）。镜像不包含 `ctr`、文档、man page、locale、APT 缓存和第三方 APT 源，也不包含 Docker 数据（`/var/lib/docker`）。

## GPU 支持

与 Docker Desktop 相同，使用 WSL2 的 GPU 半虚拟化（GPU-PV）：

1. Windows 的 NVIDIA 驱动通过 WSL 提供 `/dev/dxg` 和 `/usr/lib/wsl/lib`（`libcuda.so`、`libnvidia-ml.so`、`nvidia-smi` 等）。**镜像不包含任何 Linux 显卡驱动**，安装驱动反而会破坏 WSL 的映射。单元测试会检查 `install.sh` 中不出现 `nvidia-driver`、`cuda-drivers`、`nvidia-open`、`nvidia-kernel`。
2. `nvidia-container-cli`（libnvidia-container）是 glibc 程序，需要读取 `/usr/lib/wsl/lib` 中的 glibc 库，所以镜像必须基于 glibc，不能用 Alpine/musl。Docker Desktop 也是同样的原因。
3. 镜像在 `/etc/docker/daemon.json` 中注册 `nvidia` runtime。容器请求 GPU（`--gpus all`，或应用里的“使用全部 NVIDIA GPU”开关生成的 `DeviceRequests`）时，NVIDIA 的 hook 会把 WSL 驱动库和 `/dev/dxg` 注入容器。WSL2 不支持按序号选择 GPU。

在 RTX 4060 Laptop GPU（Windows 驱动 617.42）上实测通过：

```text
$ docker run --rm --gpus all ubuntu:24.04 nvidia-smi -L
GPU 0: NVIDIA GeForce RTX 4060 Laptop GPU (UUID: GPU-…)
$ docker run --rm --runtime nvidia -e NVIDIA_VISIBLE_DEVICES=all ubuntu:24.04 \
    nvidia-smi --query-gpu=name,driver_version,memory.total --format=csv,noheader
NVIDIA GeForce RTX 4060 Laptop GPU, 617.42, 8188 MiB
```

## 镜像约定

镜像版本号只在 `images/wsl-rootfs/VERSION` 一处定义：`build.sh` 用它给产物命名并写入镜像，`build.rs` 用它生成应用期望的版本 `rusker-rootfs-v<N>`。应用依赖镜像提供以下内容，导入后由 `VERIFY_IMAGE_SCRIPT` 检查：

- `/etc/rusker-image`：`RUSKER_IMAGE=rusker-rootfs-v<N>` 以及各组件的确切包版本；
- `/etc/rusker-managed`：内容与本地所有权记录相同（`rusker-wsl2-v1`）；
- `/etc/wsl.conf`：`systemd=false`、主机名 `rusker`、默认用户 root；
- `/usr/bin/{docker,dockerd,containerd,runc,nvidia-ctk,nvidia-container-runtime}`，插件位于 `/usr/libexec/docker/cli-plugins/`（集成发行版会从这里复制 CLI）；
- 应用脚本使用的工具：`mountpoint`/`findmnt`/`blkid`/`mkfs.ext4`（数据磁盘）、`ss`/`ip`（局域网转发）、`tar`/`sha256sum`（CLI 集成）。

应用只在 `rusker-desktop.exe` 所在目录的 `resources\wsl\rusker-rootfs-v<N>-<arch>.tar.xz` 查找镜像，找不到时提示重新安装。

## 构建

需要 Docker Buildx。在仓库根目录执行：

```sh
# amd64 + arm64（另一架构走 QEMU，较慢），输出到 dist/rootfs
images/wsl-rootfs/build.sh
# 只构建本机架构
ARCHES=amd64 images/wsl-rootfs/build.sh
```

组件版本是 `Dockerfile` 里的 `ARG`，`install.sh` 会把上游版本号解析为 APT 仓库中的确切包版本，找不到时构建失败。

发布流程（`.github/workflows/release.yml`）中，`rootfs` 任务在 Linux runner 上构建 amd64 镜像，Windows 任务通过 `makensis /DROOTFS=...` 把它打进安装包。`installer.nsi` 要求必须传入 `ROOTFS`，并在升级时删除旧版本的镜像文件。安装包只面向 x64 Windows；arm64 镜像可以构建，但目前没有对应的安装包。

### 本地开发

`cargo run` 时应用在 `target\debug\resources\wsl\` 查找镜像，安装/修复环境前先构建并放到那里：

```sh
ARCHES=amd64 images/wsl-rootfs/build.sh
mkdir -p target/debug/resources/wsl
cp dist/rootfs/rusker-rootfs-v$(cat images/wsl-rootfs/VERSION)-amd64.tar.xz target/debug/resources/wsl/
```

只验证镜像本身时可以导入为临时发行版，不影响 `rusker`：

```powershell
wsl --import rusker-imgtest $env:TEMP\rusker-imgtest dist\rootfs\rusker-rootfs-v1-amd64.tar.xz --version 2
wsl -d rusker-imgtest -u root -e sh -c "cat /etc/rusker-image; /usr/lib/wsl/lib/nvidia-smi -L"
wsl --unregister rusker-imgtest
```

## 升级镜像

修改 `images/wsl-rootfs` 后把 `VERSION` 加一，随应用一起发布。用户安装新版本后，设置页会提示镜像版本不一致；点击“修复环境”会用安装包里的新镜像替换系统盘，数据磁盘和 `daemon.json` 保留。
