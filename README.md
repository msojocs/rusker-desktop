# Rusker Desktop

Rusker Desktop 是一个使用 **Rust + [GPUI Kit](https://gpui-kit.com/)** 构建的开源容器桌面管理器。项目目标是提供类似 Docker Desktop 的常用管理体验，同时采用允许商业使用的 Apache-2.0 许可证。

> 当前处于首个可运行版本阶段：Windows 提供受管 WSL2 运行环境和 NVIDIA GPU 支持；Linux/macOS 连接已有 Docker/Podman 兼容引擎，不负责创建轻量虚拟机。

## 当前功能

- 引擎连接状态、版本及系统信息
- 容器、运行中容器和镜像数量概览
- 容器列表、关键字过滤、详情和最近 200 行日志
- 容器详情命令行（直接交互的 TTY 终端）、只读文件浏览、CPU/内存/网络/磁盘 I/O 实时趋势图及采样数据表
- 容器启动、停止、重启及带确认的删除
- 通过镜像创建并运行容器，支持名称、启动命令、TCP 端口映射和 NVIDIA GPU
- Windows 专用 WSL2 环境安装/修复、启动/停止及各发行版集成开关
- 镜像列表、关键字过滤、拉取及带确认的删除；镜像详情展示真实构建层历史、平台、标签和摘要
- 加载、空数据、操作中和错误状态
- 设置界面：主题持久化、Docker Engine 拉取代理编辑与 JSON 导出
- 系统托盘：恢复/最小化窗口、刷新、页面导航和退出菜单

详细边界与迭代计划见 [功能列表](docs/features.md)。代理导出与手动生效步骤见 [设置文档](docs/settings.md)，托盘行为见 [托盘文档](docs/tray.md)。

### 创建容器

容器页点击“创建容器”，填写本地已有镜像、可选名称、可选 CMD 和端口映射。端口格式为 `宿主机端口:容器端口`，多个映射用逗号分隔，例如 `8080:80, 8443:443`。当前仅支持 TCP，并默认绑定 `127.0.0.1`；创建时不会自动拉取缺失镜像。

## 平台支持

| 平台 | 当前目标 | 本地连接 |
| --- | --- | --- |
| Linux | 主要开发与验证平台 | Docker/Podman 兼容 Unix socket |
| Windows 10+ | 主要目标平台，使用 MSVC 工具链 | Rusker 受管 WSL2 Engine（`npipe:////./pipe/rusker_engine`），或 `DOCKER_HOST` 指定的端点 |
| macOS 15+ | 保留支持并纳入持续集成 | Docker 兼容 Unix socket |

Windows 参照 Docker Desktop 架构：在设置页一键创建 Rusker 专用 WSL2 发行版并在其中运行 Docker Engine，由应用后台持有 Engine 会话，通过 Windows named pipe 和 `/mnt/wsl` 共享 socket 提供访问，可按发行版开启 CLI/Compose 集成并支持 NVIDIA GPU。详见 [WSL2 运行环境文档](docs/wsl2.md)。macOS/Linux 轻量虚拟机管理尚未实现。

## 快速开始

### 环境要求

- Rust 1.92 或更新版本
- 可访问的 Docker/Podman 兼容 API
- GPUI Kit 所需的原生依赖，见 [开发文档](docs/development.md)

### 构建与运行

```bash
cargo run
```

常用检查：

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

### 连接引擎

默认使用 Bollard 的本地连接发现：

- Linux/macOS：通常为 `/var/run/docker.sock`
- Windows：优先使用 `//./pipe/docker_engine`；未设置 `DOCKER_HOST` 且本地连接失败时，自动探测 WSL2 中的 Linux Docker Engine
- 设置 `DOCKER_HOST` 时遵循其配置，不再回退到 WSL2

Rootless Docker 或 Podman 示例：

```bash
export DOCKER_HOST="unix:///run/user/$(id -u)/podman/podman.sock"
cargo run
```

连接 socket 需要相应操作系统权限。不要在不可信网络中使用未加密的 `tcp://` Docker API；远程 TLS/SSH 连接界面将在后续版本中提供。

## 项目结构

```text
src/
├── app.rs          # GPUI 应用状态、异步任务与主界面
├── engine/         # Tokio 执行边界、Docker API 与 WSL2 后端
├── format.rs       # 显示格式化与过滤
├── model.rs        # UI 无关的数据模型
├── volume.rs       # 卷名称/路径校验与辅助容器脚本
├── lib.rs
└── main.rs         # 原生窗口入口
docs/
├── features.md
├── architecture.md
├── development.md
└── wsl2.md
```

设计说明见 [架构文档](docs/architecture.md)。

## 开源与商业使用

本项目使用 [Apache License 2.0](LICENSE)。允许商业使用、修改和分发，但需遵守许可证中的版权、声明、NOTICE（如适用）及修改标记等条款；“允许商用”不等于“没有任何义务”。

Docker 是 Docker, Inc. 的商标。本项目不是 Docker 官方产品，与 Docker, Inc. 无隶属或背书关系。GPUI Kit、Bollard 及其他依赖保留各自许可证；发布二进制包前仍需完成第三方依赖许可证清单与合规检查。
