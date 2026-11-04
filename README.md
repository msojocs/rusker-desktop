# Rusker Desktop

Rusker Desktop 是一个使用 **Rust + [GPUI Kit](https://gpui-kit.com/)** 构建的开源容器桌面管理器。项目目标是提供类似 Docker Desktop 的常用管理体验，同时采用允许商业使用的 Apache-2.0 许可证。

> 当前处于首个可运行版本阶段：连接已有的 Docker/Podman 兼容引擎，不负责安装引擎或创建 WSL2/虚拟机。

## 当前功能

- 引擎连接状态、版本及系统信息
- 容器、运行中容器和镜像数量概览
- 容器列表、关键字过滤、详情和最近 200 行日志
- 容器启动、停止、重启及带确认的删除
- 通过镜像创建并运行容器，支持名称、启动命令和 TCP 端口映射
- 镜像列表、关键字过滤、拉取及带确认的删除
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
| Windows 10+ | 主要目标平台，使用 MSVC 工具链 | Docker 兼容 named pipe |
| macOS 15+ | 保留支持并纳入持续集成 | Docker 兼容 Unix socket |

Windows 的 WSL2 引擎自动安装、macOS/Linux 轻量虚拟机管理尚未实现。没有现成引擎时，本应用可以展示连接错误，但不会自动修改宿主机配置。

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
- Windows：通常为 `//./pipe/docker_engine`
- 设置 `DOCKER_HOST` 时遵循其配置

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
├── engine/         # Tokio 执行边界与 Docker API 实现
├── format.rs       # 显示格式化与过滤
├── model.rs        # UI 无关的数据模型
├── lib.rs
└── main.rs         # 原生窗口入口
docs/
├── features.md
├── architecture.md
└── development.md
```

设计说明见 [架构文档](docs/architecture.md)。

## 开源与商业使用

本项目使用 [Apache License 2.0](LICENSE)。允许商业使用、修改和分发，但需遵守许可证中的版权、声明、NOTICE（如适用）及修改标记等条款；“允许商用”不等于“没有任何义务”。

Docker 是 Docker, Inc. 的商标。本项目不是 Docker 官方产品，与 Docker, Inc. 无隶属或背书关系。GPUI Kit、Bollard 及其他依赖保留各自许可证；发布二进制包前仍需完成第三方依赖许可证清单与合规检查。
