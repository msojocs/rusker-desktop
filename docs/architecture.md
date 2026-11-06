# 技术架构

## 总览

首版采用单个 Cargo package，同时构建可测试的 library 和桌面 binary。领域模型及 Engine 客户端不依赖 GPUI，界面不直接暴露 Bollard 类型。

```text
┌──────────────────────────────────────────────┐
│ GPUI Kit desktop UI                          │
│ app.rs / ui                                  │
│ 页面状态、输入、确认框、Task 生命周期         │
└───────────────────┬──────────────────────────┘
                    │ owned DTO / Result
┌───────────────────▼──────────────────────────┐   ┌──────────────────────────────┐
│ EngineService                                │   │ RuntimeHostService (Windows) │
│ 持久 Tokio runtime、JoinHandle、超时          │   │ WSL2 安装/启动/停止/集成      │
└───────────────────┬──────────────────────────┘   │ Engine 会话 + named pipe 代理 │
                    │ async API                    └──────────────┬───────────────┘
┌───────────────────▼──────────────────────────┐                  │ wsl.exe
│ DockerEngine (Bollard)                       │                  ▼
│ 快照、容器操作、日志、镜像操作                │        rusker 发行版内 dockerd
└───────────────────┬──────────────────────────┘
                    │ Docker Engine API
   Unix socket / Windows named pipe（受管：//./pipe/rusker_engine）
```

## 模块边界

- `tray.rs`：原生托盘生命周期、菜单 ID 和事件到 `TrayCommand` 的转换；不包含 GPUI 或 Engine 类型。
- `settings.rs`：用户级设置模型、代理校验、持久化和 Engine JSON 片段导出；不修改系统 daemon 配置。
- `container.rs`：创建容器草稿、命令解析与统一配置校验，UI 和 Engine 共用。
- `volume.rs`：卷名称与卷内路径校验、辅助容器脚本及其输出解析，不含 I/O。
- `model.rs`：Engine、容器、镜像、卷、快照和操作结果的 owned DTO。
- `engine/mod.rs`：运行时所有权、跨执行器任务提交和统一错误类型。
- `engine/docker.rs`：Bollard 连接、查询、第三方模型转换，以及卷操作用的临时辅助容器。
- `runtime_host/mod.rs`：WSL2 运行环境服务、操作串行锁、启动与退出钩子。
- `runtime_host/wsl.rs`：专用发行版安装、所有权校验、Engine 会话（supervisor）、发行版集成与 GPU 检测。
- `runtime_host/proxy.rs`：`//./pipe/rusker_engine` → `docker system dial-stdio` 的逐连接转发。
- `format.rs`：不含 I/O 的格式化、过滤和展示逻辑。
- `app.rs`：页面、选择、忙碌状态、请求序号、通知和 GPUI Task；`app/volumes.rs` 为卷页面与卷详情。
- `main.rs`：只负责应用、资源、主题初始化和窗口创建。

后续支持更多后端时，可在 `engine` 下引入异步 trait；首版不提前建立复杂插件系统。

## 线程与异步模型

Bollard 和 WSL 子进程依赖 Tokio，GPUI 使用自己的 executor。`EngineService` 与 `RuntimeHostService` 各持有一个 `Arc<tokio::runtime::Runtime>`，Engine future 始终通过 runtime 提交，不能直接在 GPUI 前台 future 中执行 Tokio I/O。WSL 命令设置超时和 `kill_on_drop`，Windows 下使用无控制台窗口标志。运行环境操作由一把异步锁串行化；named pipe 代理作为常驻任务运行在 `RuntimeHostService` 的 runtime 上。应用退出时先由 `request_quit` 异步调用 `RuntimeHostService::shutdown`（停止 dockerd 并 `wsl --terminate rusker`，界面显示停止动画），完成后再 `cx.quit()`；其他退出路径的退出钩子仍同步调用 `shutdown_blocking` 兜底（GPUI 只为退出 future 等待约 200 ms）。

一次刷新：

1. 用户事件增加 `refresh_revision`，设置加载状态并保存 GPUI `Task`。
2. GPUI 前台任务等待 Tokio `JoinHandle`。
3. Tokio worker 完成版本、容器、镜像和系统信息查询，返回 owned `EngineSnapshot`。
4. 回到 GPUI 前台后比较 revision；过期结果被丢弃。
5. 更新 Entity 并调用 `cx.notify()`。

`Task` 必须由 View 持有。替换任务会取消旧的前台等待，但已发送到 Engine 的停止或删除操作可能已生效，因此取消界面等待不等于回滚外部副作用。所有用户操作都应通过最终刷新反映真实状态。

日志和镜像拉取响应使用有界消费：日志只请求固定尾部行数并限制单条/总展示长度；拉取完整消费响应流并检查其中的错误，不能把未消费的流留作后台泄漏。

## 卷数据操作

Docker API 没有直接读写卷内容的接口，卷的浏览、克隆和清空均通过临时辅助容器完成：

- 镜像固定为 `busybox:1.37`，缺失时自动拉取；容器带 `dev.rusker.helper` 标签、`network_mode=none`、显式 `json-file` 日志驱动，卷以 `nocopy` 挂载，结束后无论成败都强制删除。
- 浏览：卷只读挂载到 `/volume`，目标目录通过环境变量传入（不拼接 shell），一次 `find … -exec stat` 加一次 `du -d 1` 列出单层目录，界面按需逐级展开；单目录最多返回 5000 项。
- 克隆：先确认目标名不存在再创建卷，源卷只读挂载到 `/from`、新卷挂载到 `/to` 执行 `cp -a`；失败时删除新卷。
- 清空：删除卷根目录下的全部条目（含点文件），保留卷本身。
- 导入：创建但不启动辅助容器，把本地归档以流的形式通过 `PUT /containers/{id}/archive` 解压到 `/volume`；Engine 负责 gzip/bzip2/xz 解压，并保留归档中的属主。
- 导出：卷只读挂载到未启动的辅助容器，`GET /containers/{id}/archive?path=/volume/.` 的 tar 流写入目标旁的 `.partial` 文件，成功后才重命名为目标文件；`/.` 使条目相对卷根目录，导出文件可直接再导入。
- 批量删除与清理未使用卷：逐个 `force=false` 删除并汇总失败项；“未使用”以快照中所有容器（含已停止）的卷挂载为准，Engine 仍会拒绝删除在此期间被新容器引用的卷。
- 卷大小来自 `GET /system/df?type=volume&verbose=true`，与快照分开异步加载，仅在卷页面可见时请求；旧 Engine（API < 1.52）显示为“—”。

## 状态与错误

- 快照保留最后一次成功数据，刷新失败不清空可用内容。
- `busy_operation` 阻止同一窗口提交冲突操作。
- Engine 错误转换为包含操作上下文的 `EngineError`，界面不直接展示 Debug 结构。
- 连接失败、权限不足、资源不存在和 Engine 返回冲突均应可见。
- 普通单元测试不依赖本机 Docker。

## 跨平台连接

| 平台 | 默认传输 |
| --- | --- |
| Linux/macOS | `Docker::connect_with_local_defaults()`，Unix socket |
| Windows | Rusker 受管 WSL2 Engine，`//./pipe/rusker_engine` named pipe |

Windows 版参照 Docker Desktop 的 WSL2 后端：`RuntimeHostService` 相当于 `com.docker.backend`，`rusker` 专用发行版相当于 `docker-desktop` 发行版。应用持有一个长期 `wsl.exe` 会话前台运行 `dockerd`（WSL2 会在最后一个会话结束约 10 秒后终止发行版，`nohup` 后台进程不能保活）；进程内代理把 named pipe 的每个连接转发给发行版内的 `docker system dial-stdio`；`dockerd` 同时监听 `/mnt/wsl/rusker/docker.sock`，供已集成的发行版直接访问。该路径不开放 TCP 端口，也不依赖 WSL 网络模式（NAT 或 mirrored）。详见 [WSL2 运行环境文档](wsl2.md)。

`DOCKER_HOST` 可覆盖默认值；设置后 Windows 版通过 Bollard 本地默认逻辑连接该端点，并且不会自动启动受管 Engine。UI 不写死平台路径，也不把 Unix 路径作为 Windows socket 参数。后续远程连接必须单独处理 TLS、证书和凭据，不把开放 TCP 端口作为默认引导。

## 安全与数据保护

- Socket/named pipe 权限由操作系统和 Engine 管理；应用不主动提权。
- 容器删除默认 `v = false`、`force = false`；镜像删除不强制。
- 创建表单的 TCP 端口默认只绑定 `127.0.0.1`；创建后启动失败时，只 best-effort 清理本次返回的新容器 ID，不触碰已有资源。
- 启动命令使用 shell 风格引号解析为 Docker CMD 参数，不隐式经过 `/bin/sh` 执行；WSL 进程参数也逐项传递，不拼接 shell 命令。
- 停止和重启不伪装为可撤销操作，结果以 Engine 为准。
- 首版不采集遥测，不保存仓库凭据，不自动修改 Engine 配置。
- 日志可能包含秘密，只在本地展示；诊断导出功能实现前需增加脱敏提示。
- 集成测试必须显式启用，并限定 `rusker-test-*` 专用资源。

## 可测试性

纯函数测试覆盖格式化、过滤、日志截断、revision 判定，以及 WSL 列表 UTF-16 解码、rootfs 校验、受管脚本约束（不监听 TCP、不安装 Linux 显卡驱动、只移除带标记的集成文件）。Docker 与 WSL2 实机测试作为独立、默认禁用的测试目标存在（见开发文档）。UI 后续使用 GPUI test window 验证选择、确认框和异步状态；原生渲染、键盘导航与可访问性仍需 Windows/Linux/macOS 实机检查。

## 后续演进

WSL2 管理已作为独立的 `runtime_host` 层实现，与 Docker API 客户端分离；资源限制、自动更新和卸载仍待补充。macOS/Linux VM 管理应沿用同一层，不得把特权平台命令散落在 UI 回调中。
