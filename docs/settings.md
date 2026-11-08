# 设置与 Docker Engine 代理

## 设置界面

设置页仿照 Docker Desktop 新版设置：左侧为按分区纵向排列的内容，右侧为分区导航（点击跳转，滚动时高亮当前分区），底部为“放弃更改 / 应用”栏并显示是否有未保存的更改。分区如下：

- 常规：外观（跟随系统、浅色、深色），选择后立即预览，点击“应用”后持久化；“放弃更改”恢复已保存的主题。
- VM 资源：WSL 2 后端说明（CPU、内存、交换空间由 Windows 的 `.wslconfig` 管理）、磁盘镜像位置，以及 WSL2 运行环境的状态与安装/启动/停止操作。
- 资源节省：开关与阈值（30 秒到 60 分钟，默认开启、300 秒）。
- 代理：Docker Engine 拉取代理开关、`HTTP_PROXY`、`HTTPS_PROXY`、`NO_PROXY`；Engine 不由 Rusker 管理时可导出 `daemon.json` 的 `proxies` 片段。
- WSL 集成（仅 Windows）：其他 WSL2 发行版的 Docker CLI 集成开关，切换后立即生效，不需要点击“应用”。

### 磁盘镜像位置

Docker 数据磁盘 `docker-data.vhdx` 所在目录，存放镜像、容器和卷，与 Docker Desktop 的 `docker_data.vhdx` 相同。`rusker` 发行版自身的系统盘（`ext4.vhdx`）由 Rusker 系统镜像导入（见 [WSL2 系统镜像](wsl2-rootfs.md)），只包含 Docker 与 NVIDIA 运行时，固定位于应用数据目录；升级或修复时整体替换。

- 数据磁盘在首次启动 Engine 时创建（动态扩展，上限 1 TiB），位置与磁盘 ID 记录在 `runtime-host/data-disk.json`。
- 每次启动 Engine 时，Rusker 用 `wsl --mount --vhd <文件> --bare` 把数据磁盘挂到 WSL2 虚拟机，在 `rusker` 内按磁盘 WWID 找到设备，首次使用时格式化为 ext4，再绑定挂载到 `/var/lib/docker` 和 `/var/lib/containerd`。旧版本保存在系统盘上的数据会在首次挂载时自动迁移。
- 选择新目录并点击“应用”后，Rusker 停止 Engine、终止 `rusker`、用 `wsl --unmount` 只卸载这块数据磁盘，然后移动文件（同一分区内为重命名，跨分区时复制后删除原文件），更新记录并恢复 Engine。其他 WSL 发行版不受影响，也不需要 `wsl --shutdown`。目标目录中不能已有 `docker-data.vhdx`；移动失败时设置不会保存，数据磁盘仍在原位置。
- 不使用 `wsl --manage --move`：WSL 在虚拟机运行期间不会释放已停止发行版的系统盘，移动会报 `ERROR_SHARING_VIOLATION`。
- 手动停止 Engine 或退出 Rusker 时会卸载数据磁盘，文件随即可被备份或移动；进入资源节省模式时仍保持挂载，不能据此备份或移动磁盘文件。

### 资源节省

Windows WSL2 后端开启后，受管 Engine 在**没有运行中的容器、没有 Windows 管道客户端连接且未启用任何 WSL 集成**达到阈值时，向 `dockerd` 及其直接管理的 `containerd` 发送 `SIGSTOP`，状态栏显示“资源节省模式”。这是暂停而非冷停：保留长期 WSL 会话、`rusker` 发行版和数据 VHD 挂载，不终止发行版、不卸载磁盘。

- 通过 `npipe:////./pipe/rusker_engine` 到达的下一请求（`docker.exe`、Rusker 自身刷新等）会在全局操作锁下发送 `SIGCONT` 恢复暂停的进程，等待 Docker API 就绪后再转发原请求。
- 拉取镜像、`docker logs -f` 等仍在进行的管道连接会阻止进入资源节省模式。
- 手动“停止”或退出 Rusker 仍正常优雅关闭 Engine、终止发行版并卸载数据磁盘；手动停止后，客户端请求不会自动启动 Engine。
- 启用任意 WSL 集成时不自动进入资源节省模式：集成发行版直连 `/mnt/wsl/rusker/docker.sock`，不能触发 Windows 侧唤醒，其长时间操作也不计入 Windows 管道活动连接。
- 修改开关或阈值后点击“应用”立即生效，不会重启 Engine；关闭资源节省开关会恢复已暂停的 Engine。
- 暂停可减少空闲 CPU 活动，但不保证释放 WSL 内存。建议按需配置 WSL 的 `autoMemoryReclaim`；Rusker 不会自动修改用户全局 `.wslconfig`。

该行为对齐 Docker 官方 Windows WSL 后端的 Resource Saver 语义（暂停 Engine，而非关闭整个 WSL 环境）；`SIGSTOP`/`SIGCONT` 是 Rusker 的实现方式，不代表 Docker 官方采用相同实现。

### 其他
默认配置目录由操作系统决定，可通过 `RUSKER_CONFIG_DIR` 覆盖。目录内包含：

- `settings.json`：Rusker 自身设置。
- `docker-proxies.json`：导出的 Engine 代理片段。

Unix 下新写入的设置文件权限为 `0600`。代理地址不支持内嵌用户名或密码，避免在明文凭据配置中保存秘密。

## 受管 Engine：与 Docker Desktop 相同的代理结构

Windows 上未设置 `DOCKER_HOST` 时，Engine 是 Rusker 自己的 WSL2 Engine（见 [WSL2 文档](wsl2.md)）。Docker Desktop 不会把用户填写的代理地址交给 VM 内的 dockerd：`docker info` 始终显示 `HTTP/HTTPS Proxy: http.docker.internal:3128`，这是 VM 内的入口，连接到达 Windows 上的 `com.docker.backend`，由它内置的 HTTP 代理按设置逐个请求决定走上游代理还是直连。Rusker 采用同样的结构：

```text
dockerd（rusker 发行版）
  → http://127.0.0.1:23128           VM 内转发入口（rusker-vm-proxy，仅监听 VM 回环）
  → 长期 wsl.exe 会话的 stdin/stdout  多路复用帧，不经过网络
  → RuntimeHostService（Windows）     完整的 HTTP 正向代理，逐请求决定路由
  → 上游代理（如 Windows 本机 127.0.0.1:7890）或直接连接目标
```

### dockerd 一侧

- `daemon.json` 的 `proxies` 固定为 `http-proxy`/`https-proxy` = `http://127.0.0.1:23128`，与代理模式无关（“不使用代理”时也一样，请求由 Windows 直接发出）；`docker info` 中的 HTTP/HTTPS Proxy 显示该地址。
- 不写 `no-proxy`：与 Docker Desktop 相同，`NO_PROXY` 在 Windows 一侧判断。localhost 与回环地址由 dockerd 自身直连。
- Rusker 在启动 Engine 前把 `proxies` 合并进 `/etc/docker/daemon.json`，只替换该键，保留 NVIDIA runtime 等其他配置；文件不是合法 JSON 对象时拒绝修改并报错。旧版本写入的原始代理地址会在第一次启动时被替换（此时重启一次 dockerd）。
- 因此**修改代理设置不会改动 `daemon.json`，也不会重启 Engine**，点击“应用”后下一个请求就使用新设置。

### Windows 一侧（逐请求）

- 每个请求都重新解析当前设置；“系统代理”模式每次重新读取 Windows 系统代理，系统代理的开关和地址变化立即生效。
- `NO_PROXY` 按 Go 的规则匹配：`*`；IP 与 CIDR；`example.com` 匹配自身和子域名，`.example.com`/`*.example.com` 只匹配子域名；条目可带 `:端口`。
- `CONNECT host:port`（访问 `https://` 仓库）：有 HTTPS 上游时把 `CONNECT` 交给上游，由上游应答；没有上游或被 `NO_PROXY` 排除时由 Windows 直接建立隧道并回复 `200`。
- 纯 HTTP 请求（`GET http://host/...`）：有 HTTP 上游时原样转给上游；否则改写为普通路径形式、去掉 `Proxy-*` 与逐跳头后直接连接目标。客户端连接保持长连接，同一连接上的请求可以发往不同主机；连续发往同一目的地时复用出站连接。
- 上游代理支持 `http://` 与 `https://`（与代理之间使用 TLS，按 Windows 证书存储校验，企业根证书可用）。
- 转发与 WSL 网络模式（NAT/mirrored）无关，不开放 Windows 端口，不需要代理软件开启“允许局域网连接”，也不需要防火墙规则或管理员权限（Docker Desktop 使用 Hyper-V socket，Rusker 改用 `wsl.exe` 标准输入输出）。
- 上游或目标不可达时，dockerd 收到 `502 Bad Gateway`（拉取报错中可见）；转发进程异常退出时，设置页显示警告并自动重启。
- 该转发只用于 Engine 自身的出站请求（拉取、推送、登录、构建时拉取基础镜像）。容器内的代理环境变量（“容器代理”）仍原样注入，容器内的 `127.0.0.1` 指容器自身。

## 外部 Engine：手动应用

Linux、macOS，或 Windows 上设置了 `DOCKER_HOST` 时，Engine 不由 Rusker 管理，Rusker 不会修改系统 daemon 配置或重启服务，导出的片段也是填写的原始地址（不经上述转发）；该 Engine 运行在 VM 中时，地址需要从 VM 内可达。此时需要手动完成：

1. 导出 `docker-proxies.json`。
2. 将其中的 `proxies` 对象合并到 Docker `daemon.json`，保留其他已有配置。
3. 检查 JSON 语法。
4. 按正常运维流程重启 Docker Engine。
5. 在 Rusker 中重新拉取镜像验证网络。

示例片段：

```json
{
  "proxies": {
    "http-proxy": "http://127.0.0.1:8080",
    "https-proxy": "http://127.0.0.1:8080",
    "no-proxy": "localhost,127.0.0.1,::1"
  }
}
```

禁用代理时导出的片段为：

```json
{
  "proxies": {}
}
```

合并时应替换整个 `proxies` 对象，而不是覆盖整个 `daemon.json`。

## Linux

本机 Docker Engine 常见配置路径为 `/etc/docker/daemon.json`。请先备份，再使用编辑器合并配置：

```bash
sudo cp /etc/docker/daemon.json /etc/docker/daemon.json.backup
sudoedit /etc/docker/daemon.json
sudo systemctl restart docker
```

如果文件不存在，应确认发行版的 Docker 配置方式；不要直接覆盖已有文件。使用 Podman 时，此 JSON 不适用，需要配置 Podman 自身的网络环境。

## Windows（外部 Engine）

设置 `DOCKER_HOST` 连接其他 Engine 时，独立 Docker Engine 常见配置路径为 `C:\ProgramData\Docker\config\daemon.json`。Docker Desktop、Rancher Desktop 或其他兼容 Engine 可能使用各自的配置界面和虚拟机网络，不应直接假设该路径有效。

请通过对应产品的受支持方式合并代理并重启 Engine。Rusker 只修改自己的 `rusker` 发行版，不会修改 Windows 服务、注册表或其他 WSL2 发行版的 Docker 配置。

## 代理字段

- `HTTP_PROXY` / `HTTPS_PROXY`：仅支持带主机名的 `http://`、`https://` 地址，不允许路径、查询参数、片段或内嵌凭据。
- `NO_PROXY`：逗号分隔的主机名、域名、IP 或 CIDR，例如 `localhost,127.0.0.1,::1,.internal.example.com`。
- 启用代理时，HTTP 和 HTTPS 地址至少填写一个。

这些设置面向 **Docker Engine 的出站请求**，不是 Rusker 应用代理，也不会自动注入已有或新建容器的环境变量。受管 Engine 中，这些地址由 Rusker 在 Windows 上使用（见上文“受管 Engine”）。
