# 设置与 Docker Engine 代理

## 设置界面

设置页仿照 Docker Desktop 新版设置：左侧为按分区纵向排列的内容，右侧为分区导航（点击跳转，滚动时高亮当前分区），底部为“放弃更改 / 应用”栏并显示是否有未保存的更改。分区如下：

- 常规：外观（跟随系统、浅色、深色），选择后立即预览，点击“应用”后持久化；“放弃更改”恢复已保存的主题。
- VM 资源：WSL 2 后端说明（CPU、内存、交换空间由 Windows 的 `.wslconfig` 管理）、磁盘镜像位置，以及 WSL2 运行环境的状态与安装/启动/停止操作。
- 资源节省：开关与阈值（30 秒到 60 分钟，默认开启、300 秒）。
- 代理：Docker Engine 拉取代理开关、`HTTP_PROXY`、`HTTPS_PROXY`、`NO_PROXY`；Engine 不由 Rusker 管理时可导出 `daemon.json` 的 `proxies` 片段。
- WSL 集成（仅 Windows）：其他 WSL2 发行版的 Docker CLI 集成开关，切换后立即生效，不需要点击“应用”。

### 磁盘镜像位置

受管发行版 `rusker` 的虚拟磁盘（`ext4.vhdx`）所在目录，实际位置从 `HKCU\Software\Microsoft\Windows\CurrentVersion\Lxss` 中该发行版的 `BasePath` 读取。

- 尚未安装时，选择的目录作为安装位置（必须为空目录）。
- 已安装时，选择一个空目录并点击“应用”，Rusker 会停止 Engine、执行 `wsl --manage rusker --move <目录>`，成功后再保存设置并恢复 Engine。需要 WSL 2.3 或更新版本；移动失败时设置不会保存。

### 资源节省

开启后，受管 Engine 在**没有运行中的容器且没有 Windows 管道客户端连接**达到阈值时，停止 dockerd 并终止 `rusker` 发行版以释放 CPU 与内存，状态栏显示“资源节省模式”。

- 任何连接 `npipe:////./pipe/rusker_engine` 的客户端（`docker.exe`、Rusker 自身刷新等）都会让 Engine 自动恢复，连接会等待 Engine 就绪后再转发。
- 拉取镜像、`docker logs -f` 等仍在进行的管道连接会阻止进入资源节省模式。
- 手动“停止”不属于资源节省，客户端连接不会自动启动 Engine。
- 已知限制：资源节省模式下，已集成的 WSL 发行版内通过 `/mnt/wsl/rusker/docker.sock` 运行的 docker 命令无法唤醒 Engine，需要先通过 Windows 端点或 Rusker 恢复；直接连接该 socket 的长时间操作也不会被计为活动连接。
- 修改开关或阈值后点击“应用”立即生效，不会重启 Engine。

### 其他
默认配置目录由操作系统决定，可通过 `RUSKER_CONFIG_DIR` 覆盖。目录内包含：

- `settings.json`：Rusker 自身设置。
- `docker-proxies.json`：导出的 Engine 代理片段。

Unix 下新写入的设置文件权限为 `0600`。代理地址不支持内嵌用户名或密码，避免在明文凭据配置中保存秘密。

## 受管 Engine：保存即生效

Windows 上未设置 `DOCKER_HOST` 时，Engine 是 Rusker 自己的 WSL2 Engine（见 [WSL2 文档](wsl2.md)），代理行为与 Docker Desktop 的 “Apply & restart” 一致：

1. 点击“应用”后，Rusker 把 `proxies` 合并进 `rusker` 发行版的 `/etc/docker/daemon.json`。只替换 `proxies` 键，保留 NVIDIA runtime 等其他配置；关闭代理时删除该键。文件不是合法 JSON 对象时拒绝修改并报错。
2. 配置有变化且 Engine 正在运行时，优雅重启 `dockerd`（运行中的容器会停止，设置了重启策略的容器会自动恢复）。配置无变化时不重启。
3. Engine 未运行时，配置在下次启动 Engine 前写入；应用启动时也会按已保存的设置校正 `daemon.json`。

WSL2 使用 NAT 网络时，`127.0.0.1` 指向 WSL 自身，代理地址应填写 Windows 主机地址；使用 `networkingMode=mirrored` 时可直接使用 `127.0.0.1`。

## 外部 Engine：手动应用

Linux、macOS，或 Windows 上设置了 `DOCKER_HOST` 时，Engine 不由 Rusker 管理，Rusker 不会修改系统 daemon 配置或重启服务。此时需要手动完成：

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

这些设置面向 **Docker Engine 的出站请求**，不是 Rusker 应用代理，也不会自动注入已有或新建容器的环境变量。
