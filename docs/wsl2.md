# Windows WSL2 运行环境与 NVIDIA GPU

Windows 默认使用 Rusker 自己创建和管理的 WSL2 环境，不自动复用 Docker Desktop、现有 named pipe 或其他发行版中的 Docker Engine。

## 架构与生命周期

整体结构参照 Docker Desktop 的 WSL2 后端：

| Docker Desktop | Rusker | 说明 |
| --- | --- | --- |
| `com.docker.backend` | 应用内 `RuntimeHostService` | 持有 Engine 会话、named pipe 代理和安装/集成逻辑 |
| `docker-desktop` 发行版（`docker-desktop.iso`） | `rusker` 发行版（Rusker 系统镜像） | 运行 `dockerd`；系统盘是预装 Docker 与 NVIDIA 运行时的精简镜像，可整体替换 |
| `docker_data.vhdx` | `docker-data.vhdx` | 独立数据磁盘，`wsl --mount --bare` 挂载到 `/var/lib/docker`、`/var/lib/containerd`，可单独移动 |
| `\\.\pipe\docker_engine` | `\\.\pipe\rusker_engine` | Windows 侧 Docker API 端点 |
| `http.docker.internal:3128` | `http://127.0.0.1:23128`（`rusker` 内） | Engine 拉取代理入口，连接转到 Windows 后再连上游代理 |
| `/mnt/wsl/docker-desktop/...` | `/mnt/wsl/rusker/docker.sock` | WSL2 发行版之间共享的 tmpfs，供已集成发行版访问 |
| `cli-tools` + `cli-plugins` 链接 | `/usr/local/lib/rusker` + `cli-plugins` 链接 | 集成发行版内的 CLI 与 Compose/Buildx |

```text
Windows                                   WSL2 utility VM
┌──────────────────────────────┐          ┌──────────────────────────────────────┐
│ Rusker Desktop               │ wsl.exe  │ rusker 发行版                         │
│  Engine 会话 ────────────────┼─────────►│  supervisor → dockerd（前台）         │
│  \\.\pipe\rusker_engine      │ 常驻会话 │   ├ unix:///var/run/docker.sock       │
│   └ 代理 ────────────────────┼─────────►│   │  ▲ rusker-vm-proxy --docker-api    │
│ docker.exe / Bollard ─► pipe │ 常驻复用 │   └ unix:///mnt/wsl/rusker/docker.sock│
└──────────────────────────────┘          │ Ubuntu 等已集成发行版 ──────────────►┘│
                                          └──────────────────────────────────────┘
```

设置页的“WSL2 运行环境”提供安装/修复、启动、停止和状态刷新：

1. 使用安装包自带的 **Rusker 系统镜像**（`<安装目录>\resources\wsl\rusker-rootfs-v<N>-amd64.tar.xz`，对应 Docker Desktop 的 `resources\docker-desktop.iso`），不联网下载；找不到时提示重新安装。镜像基于 Debian 12，预装官方 Docker Engine、Compose、Buildx 与 NVIDIA Container Toolkit，构建方式见 [WSL2 系统镜像](wsl2-rootfs.md)。
2. 通过 `wsl.exe --import` 创建名为 `rusker` 的 WSL2 专用发行版，并校验镜像版本与组件。安装过程不在发行版内联网安装任何软件。
3. **修复环境 / 升级**与 Docker Desktop 一样整体替换系统盘：先确认 Docker 数据已在数据磁盘上（旧版本会先迁移），保存 `/etc/docker/daemon.json`，然后注销旧系统盘、导入当前镜像，再恢复 `daemon.json`（缺少的 `nvidia` runtime 会从新镜像补上）。容器、镜像和卷保留；在 `rusker` 发行版里手动安装的软件不保留。镜像版本与应用要求不一致时，设置页会提示点击“修复环境”。
4. 应用通过一个长期运行的 `wsl.exe` 会话在前台运行 `dockerd`，不依赖发行版内的 systemd。WSL2 会在最后一个会话结束约 10 秒后终止发行版，后台 `nohup` 进程无法保活，因此会话必须由应用持有。如果发行版中已有上一次留下的 `dockerd`，supervisor 会接管它，而不会再启动第二个。
5. Engine **不监听 TCP 端口**。Windows 侧由应用内代理提供 `\\.\pipe\rusker_engine`，一个长期 `wsl.exe` 会话运行嵌入的 `rusker-vm-proxy --docker-api`，将各管道连接多路复用到发行版内独立的 `/var/run/docker.sock` 连接；该路径与 WSL 网络模式（NAT/mirrored）无关，并正确传递 `docker run -i`/`exec -i` 的 stdin EOF。空闲常驻通道不算业务连接；活跃的管道客户端仍阻止资源节省。资源节省模式下，下一请求会在全局操作锁下恢复暂停的 Engine，等待 Docker API 就绪后再转发原请求；手动停止后不会因客户端请求而自动启动。
6. **绑定挂载与 Docker Desktop 的 WSL2 后端一致，不需要配置文件共享**：WSL 自动把所有盘符挂到 `rusker` 发行版的 `/mnt/<盘符>`，因此任意 Windows 目录都能挂载。管道代理会改写 `containers/create` 请求中 `-v`（`HostConfig.Binds`）与 `--mount type=bind`（`HostConfig.Mounts`）的 Windows 源路径，例如 `docker run -v C:\Users\me\src:/src` 中的 `C:\Users\me\src`（或 `C:/Users/me/src`）会变为 `/mnt/c/Users/me/src`；命名卷、Linux 路径和 UNC 路径保持不变。attach/exec 等升级后的连接及其他请求原样转发。已集成的 WSL 发行版直连 socket，应直接使用 `/mnt/c/...` 形式。
7. **Engine 代理与 Docker Desktop 一致，在 Windows 上逐请求解析**：dockerd 的代理固定为 `rusker` 内的 `http://127.0.0.1:23128`（对应 `http.docker.internal:3128`）；Engine 会话存活期间，应用另持有一个 `wsl.exe` 会话运行转发程序 `rusker-vm-proxy`（随应用嵌入的静态 Linux 程序，首次运行时安装到 `/usr/local/libexec/rusker/<内容哈希>/`，升级后自动替换），把每个连接以多路复用帧经该会话的 stdio 交给 Windows 上的内置 HTTP 代理，由它按当前设置与 `NO_PROXY` 决定走上游代理还是直连。因此 NAT 模式下填写 `127.0.0.1:7890` 也指 Windows 本机，修改代理不重启 Engine，也不需要开放端口或防火墙规则。详见[设置文档](settings.md#受管-engine与-docker-desktop-相同的代理结构)。

生命周期与 Docker Desktop 一致：

- 应用启动时，若受管环境已安装，会自动启动 Engine 并恢复已保存的发行版集成；设置了 `DOCKER_HOST` 时不自动启动。
- “停止”先向 `dockerd` 发送 SIGTERM（容器按 Docker 的关闭超时停止），再终止 `rusker` 发行版。随后卸载 Docker 数据磁盘。容器和镜像保存在数据磁盘中，下次启动不会重建环境。
- 退出应用（托盘“退出 Rusker”，或无托盘时关闭窗口）会以同样方式优雅停止本应用持有的 Engine 并终止 `rusker` 发行版，期间界面显示停止动画；Windows 侧 named pipe 也随之消失。应用异常退出时 `wsl.exe` 会话可能残留；下次启动会接管仍在运行的 `dockerd`。
- “修复环境”会先优雅停止 Engine，再用当前系统镜像替换系统盘（见上文第 3 步），数据磁盘不受影响。
- **Resource Saver 是暂停，不是冷停**：启用资源节省后，没有运行中的容器和 Windows 管道客户端连接、且未启用任何 WSL 集成，空闲达到阈值时，Rusker 向 `dockerd` 及其直接管理的 `containerd` 发送 `SIGSTOP`；长期 WSL 会话、`rusker` 发行版和数据 VHD 挂载均保留，不终止发行版、不卸载磁盘。Windows named pipe 的下一请求在全局操作锁下通过 `SIGCONT` 恢复 Engine，等待 Docker API 就绪后再转发原请求；关闭资源节省开关也会恢复已暂停的 Engine。手动停止和退出仍走上述正常关闭流程。
- 启用任意 WSL 集成时不自动进入资源节省模式，因为集成发行版直连共享 socket，不能触发 Windows 侧唤醒。暂停不保证释放 WSL 内存，建议按需启用 WSL `autoMemoryReclaim`；Rusker 不会自动修改用户全局 `.wslconfig`。这对齐 Docker 官方 Windows WSL 后端的 Resource Saver 语义，但不宣称 Docker 官方也使用 `SIGSTOP`/`SIGCONT`。详见[资源节省设置](settings.md#资源节省)。

Windows 下的 `docker.exe` 可以这样使用受管 Engine：

```powershell
docker context create rusker --docker "host=npipe:////./pipe/rusker_engine"
docker context use rusker
# 或单次：docker -H npipe:////./pipe/rusker_engine ps
```

Docker API 通道在首次连接时启动，随后每个新管道连接只打开一个复用流，不再逐连接启动 WSL 或 Docker CLI。首次启动可能包含检查/安装嵌入 helper 的成本；通道失败会关闭已有流，下一新连接重新建立通道，不重放原请求。应用内客户端复用版本协商结果，Windows 管道连接池最多保留 4 个空闲连接并在 5 秒后回收，避免长期阻止资源节省。同一 Windows 用户只能有一个 Rusker 实例持有该 named pipe，冲突时会在设置页显示警告。

安装环境需要可用的 WSL2，不需要网络：系统镜像随安装包分发。WSL 功能本身的启用及 Windows 重启不由应用自动执行。

### 所有权与数据保护

- 专用发行版必须同时具有 Rusker 本地所有权记录和发行版内 `/etc/rusker-managed` 标记。
- 如果检测到同名 `rusker` 发行版但没有所有权记录，所有管理操作都会停止，避免误接管或覆盖用户环境。
- 保存中的 `daemon.json.preserved`、系统盘（`distro/ext4.vhdx`）、数据磁盘记录（`data-disk.json`）和所有权记录位于应用数据目录的 `runtime-host` 下，数据磁盘默认位于 `runtime-host/disk`，可在设置中移动（见[设置文档](settings.md#磁盘镜像位置)）；开发/测试可用 `RUSKER_DATA_DIR` 覆盖数据根目录。
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
- 受管环境安装成功（系统镜像已预装 NVIDIA Container Toolkit 并注册 `nvidia` runtime）。

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
- 安装中断后使用“修复环境”；不要手动用 `wsl --import` 导入其他 rootfs 覆盖现有虚拟磁盘。
- 当前不包含资源限制、自动更新、界面卸载、文件共享和防火墙管理。
