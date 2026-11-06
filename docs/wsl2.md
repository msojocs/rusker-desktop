# Windows WSL2 运行环境与 NVIDIA GPU

Windows 默认使用 Rusker 自己创建和管理的 WSL2 环境，不自动复用 Docker Desktop、现有 named pipe 或其他发行版中的 Docker Engine。

## 架构与生命周期

整体结构参照 Docker Desktop 的 WSL2 后端：

| Docker Desktop | Rusker | 说明 |
| --- | --- | --- |
| `com.docker.backend` | 应用内 `RuntimeHostService` | 持有 Engine 会话、named pipe 代理和安装/集成逻辑 |
| `docker-desktop` 发行版 | `rusker` 发行版 | 运行 `dockerd`，数据保存在其虚拟磁盘 |
| `\\.\pipe\docker_engine` | `\\.\pipe\rusker_engine` | Windows 侧 Docker API 端点 |
| `/mnt/wsl/docker-desktop/...` | `/mnt/wsl/rusker/docker.sock` | WSL2 发行版之间共享的 tmpfs，供已集成发行版访问 |
| `cli-tools` + `cli-plugins` 链接 | `/usr/local/lib/rusker` + `cli-plugins` 链接 | 集成发行版内的 CLI 与 Compose/Buildx |

```text
Windows                                   WSL2 utility VM
┌──────────────────────────────┐          ┌──────────────────────────────────────┐
│ Rusker Desktop               │ wsl.exe  │ rusker 发行版                         │
│  Engine 会话 ────────────────┼─────────►│  supervisor → dockerd（前台）         │
│  \\.\pipe\rusker_engine      │ 每连接   │   ├ unix:///var/run/docker.sock       │
│   └ 代理 ────────────────────┼─────────►│   │  ▲ docker system dial-stdio       │
│ docker.exe / Bollard ─► pipe │          │   └ unix:///mnt/wsl/rusker/docker.sock│
└──────────────────────────────┘          │ Ubuntu 等已集成发行版 ──────────────►┘│
                                          └──────────────────────────────────────┘
```

设置页的“WSL2 运行环境”提供安装/修复、启动、停止和状态刷新：

1. 下载固定版本的 Ubuntu 24.04 rootfs，并使用官方 SHA-256 校验。当前支持 `x86_64` 和 `aarch64`，安装源与校验值固定在代码中。
2. 通过 `wsl.exe --import` 创建名为 `rusker` 的 WSL2 专用发行版。
3. 在专用发行版内通过 Ubuntu 官方仓库安装 Docker Engine、Compose v2 与 Buildx，通过 NVIDIA 官方签名 APT 仓库安装固定版本的 NVIDIA Container Toolkit。
4. 应用通过一个长期运行的 `wsl.exe` 会话在前台运行 `dockerd`，不依赖发行版内的 systemd。WSL2 会在最后一个会话结束约 10 秒后终止发行版，后台 `nohup` 进程无法保活，因此会话必须由应用持有。如果发行版中已有上一次留下的 `dockerd`，supervisor 会接管它，而不会再启动第二个。
5. Engine **不监听 TCP 端口**。Windows 侧由应用内代理提供 `\\.\pipe\rusker_engine`，每个连接转发到发行版内的 `docker system dial-stdio`；该路径与 WSL 网络模式（NAT/mirrored）无关，并正确传递 `docker run -i`/`exec -i` 的 stdin EOF。只有 Engine 会话存活时代理才转发，不会因零散连接而唤醒发行版。

生命周期与 Docker Desktop 一致：

- 应用启动时，若受管环境已安装，会自动启动 Engine 并恢复已保存的发行版集成；设置了 `DOCKER_HOST` 时不自动启动。
- “停止”先向 `dockerd` 发送 SIGTERM（容器按 Docker 的关闭超时停止），再终止 `rusker` 发行版。容器和镜像保存在虚拟磁盘中，下次启动不会重建环境。
- 退出应用（托盘“退出 Rusker”，或无托盘时关闭窗口）会以同样方式优雅停止本应用持有的 Engine 并终止 `rusker` 发行版，期间界面显示停止动画；Windows 侧 named pipe 也随之消失。应用异常退出时 `wsl.exe` 会话可能残留；下次启动会接管仍在运行的 `dockerd`。
- “修复环境”会先优雅停止 Engine，再重跑安装脚本。

Windows 下的 `docker.exe` 可以这样使用受管 Engine：

```powershell
docker context create rusker --docker "host=npipe:////./pipe/rusker_engine"
docker context use rusker
# 或单次：docker -H npipe:////./pipe/rusker_engine ps
```

每个新连接都要启动一次 `wsl.exe`，因此单条 CLI 命令会多出约 0.3–0.8 秒；应用内的 Bollard 客户端会复用连接。同一 Windows 用户只能有一个 Rusker 实例持有该 named pipe，冲突时会在设置页显示警告。

安装需要可用的 WSL2、Windows 自带 `curl.exe` 和互联网连接。WSL 功能本身的启用及 Windows 重启不由应用自动执行。

### 所有权与数据保护

- 专用发行版必须同时具有 Rusker 本地所有权记录和发行版内 `/etc/rusker-managed` 标记。
- 如果检测到同名 `rusker` 发行版但没有所有权记录，所有管理操作都会停止，避免误接管或覆盖用户环境。
- rootfs、虚拟磁盘和所有权记录位于应用数据目录的 `runtime-host` 下；开发/测试可用 `RUSKER_DATA_DIR` 覆盖数据根目录。
- 当前没有界面卸载功能。不要仅删除数据目录来卸载，否则会丢失所有权记录；也不要手动把其他发行版重命名为 `rusker`。

## 各发行版集成开关

设置页列出其他 WSL2 发行版，可分别启用或停用集成。启用时：

1. 以 tar 流把受管 Docker CLI 及 Compose/Buildx 插件从专用发行版复制到目标发行版的 `/usr/local/lib/rusker/`（不在内存中缓存整个文件）。内容指纹未变化时跳过复制。
2. 创建 `/usr/local/bin/docker` 包装器；未显式设置 `DOCKER_HOST`/`DOCKER_CONTEXT` 时，默认使用 `unix:///mnt/wsl/rusker/docker.sock`。
3. 在 `/usr/local/lib/docker/cli-plugins/` 下为 `docker-compose`、`docker-buildx` 创建指向 Rusker 文件的符号链接；该位置已有同名插件（包括 Docker Desktop 的链接）时保留原文件。
4. 写入 `/usr/local/lib/rusker/integrated` 标记并持久化 Rusker 设置。

已保存的集成会在受管环境安装或启动后恢复。启用集成要求受管 Engine 已就绪。旧版本（TCP 包装器）的集成会在下次启动时自动升级为 socket 方式。

共享 socket 的权限为 `0666`，与 Docker Desktop 集成发行版的行为相当：WSL2 中任意 Windows 进程本就可以通过 `wsl.exe -u root` 获得 root，发行版内的 Linux 用户不是安全边界。SDK 或工具若不经过 `docker` CLI，可设置 `DOCKER_HOST=unix:///mnt/wsl/rusker/docker.sock`。

保护规则：

- 已存在或被第三方修改的 `/usr/local/bin/docker` 不会被覆盖，启用会失败并显示原因。
- 停用时只删除带 Rusker 标记的包装器、Rusker 自己的 CLI 文件，以及仍指向 Rusker 文件的插件链接；如果包装器已被修改，会保留并报告。
- 不删除目标发行版已有 `/usr/bin/docker`、其他插件、容器、镜像或 Docker 配置。

**开关不是安全访问控制。** WSL2 发行版共享同一个 utility VM 和 `/mnt/wsl`，有权限的用户仍可手动访问共享 socket 或专用发行版。开关控制的是默认 Docker CLI 集成。受管 Engine 不监听任何 TCP 端口；不要手动为 `dockerd` 添加 `tcp://` 监听或开放 Windows 防火墙端口。

显式设置的 `DOCKER_HOST` 仍是 Rusker 的高级覆盖项；设置后 Rusker 使用该端点，不连接默认受管地址。包装器同样尊重目标发行版中已有的 `DOCKER_HOST`。

## NVIDIA GPU

实现方式与 Docker Desktop 的边界一致：**NVIDIA GPU 驱动由 Windows 宿主机提供，不在 WSL 基础环境中安装显卡驱动。** Docker Desktop 官方文档同样将支持 WSL2 GPU-PV 的当前 NVIDIA 驱动列为前置条件。

前置条件：

- NVIDIA GeForce/RTX GPU 和支持 WSL2 GPU-PV 的当前 Windows 驱动；
- 最新 WSL2 内核；
- 受管环境安装成功，NVIDIA Container Toolkit 已注册为 Docker runtime。

安装器不会安装 `nvidia-driver`、`cuda-drivers` 或 `nvidia-open`，这些 Linux 驱动可能破坏 Windows 驱动提供的 WSL 映射。

状态检测包括：

- `/dev/dxg` 是否存在；
- `/usr/lib/wsl/lib/nvidia-smi` 是否能读取 GPU；
- Docker 是否注册 `nvidia` runtime。

创建容器时启用“使用全部 NVIDIA GPU”，会生成等价于 `docker run --gpus all` 的 Docker `DeviceRequests`。WSL2 不支持按 GPU 序号选择，因此当前不提供设备选择器。检测不可用时会禁用该开关；Engine 返回错误时不会静默降级到 CPU。

状态检测不会拉取镜像。需要实际验证 CUDA 容器时，可显式执行以下命令（会拉取 NVIDIA 示例镜像并创建临时容器）：

```powershell
wsl.exe --distribution rusker --exec /usr/bin/docker run --rm --gpus all nvcr.io/nvidia/k8s/cuda-sample:nbody nbody -gpu -benchmark
```

## 故障排查

- Engine 日志：`wsl.exe --distribution rusker --exec /bin/cat /var/log/rusker-dockerd.log`（超过 10 MB 时轮转为 `.1`；启动失败时设置页会显示最近 20 行）。
- 检查 Windows 端点：`docker -H npipe:////./pipe/rusker_engine version`。
- 检查共享 socket：在已集成发行版中执行 `ls -l /mnt/wsl/rusker/docker.sock`。
- 检查 GPU：`wsl.exe --distribution rusker --exec /usr/lib/wsl/lib/nvidia-smi`。
- 检查 Docker runtime：`wsl.exe --distribution rusker --exec /usr/bin/docker info`。
- 设置页提示“没有由本应用持有的 Engine 会话”：通常是上一次异常退出遗留的 `dockerd`，点击“启动”即可接管。
- 安装中断后使用“修复环境”；不要通过导入其他 rootfs 覆盖现有虚拟磁盘。
- 当前不包含资源限制、自动更新、界面卸载、文件共享和防火墙管理。
