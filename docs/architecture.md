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
┌───────────────────▼──────────────────────────┐
│ EngineService                                │
│ 持久 Tokio runtime、JoinHandle、超时          │
└───────────────────┬──────────────────────────┘
                    │ async API
┌───────────────────▼──────────────────────────┐
│ DockerEngine (Bollard)                       │
│ 快照、容器操作、日志、镜像操作                │
└───────────────────┬──────────────────────────┘
                    │ Docker Engine API
          Unix socket / Windows named pipe
```

## 模块边界

- `tray.rs`：原生托盘生命周期、菜单 ID 和事件到 `TrayCommand` 的转换；不包含 GPUI 或 Engine 类型。
- `settings.rs`：用户级设置模型、代理校验、持久化和 Engine JSON 片段导出；不修改系统 daemon 配置。
- `container.rs`：创建容器草稿、命令解析与统一配置校验，UI 和 Engine 共用。
- `model.rs`：Engine、容器、镜像、快照和操作结果的 owned DTO。
- `engine/mod.rs`：运行时所有权、跨执行器任务提交和统一错误类型。
- `engine/docker.rs`：Bollard 连接、查询和第三方模型转换。
- `format.rs`：不含 I/O 的格式化、过滤和展示逻辑。
- `app.rs`：页面、选择、忙碌状态、请求序号、通知和 GPUI Task。
- `main.rs`：只负责应用、资源、主题初始化和窗口创建。

后续支持更多后端时，可在 `engine` 下引入异步 trait；首版不提前建立复杂插件系统。

## 线程与异步模型

Bollard 依赖 Tokio，GPUI 使用自己的 executor。`EngineService` 持有一个 `Arc<tokio::runtime::Runtime>`，Engine future 始终通过该 runtime 提交，不能直接在 GPUI 前台 future 中执行 Tokio I/O。

一次刷新：

1. 用户事件增加 `refresh_revision`，设置加载状态并保存 GPUI `Task`。
2. GPUI 前台任务等待 Tokio `JoinHandle`。
3. Tokio worker 完成版本、容器、镜像和系统信息查询，返回 owned `EngineSnapshot`。
4. 回到 GPUI 前台后比较 revision；过期结果被丢弃。
5. 更新 Entity 并调用 `cx.notify()`。

`Task` 必须由 View 持有。替换任务会取消旧的前台等待，但已发送到 Engine 的停止或删除操作可能已生效，因此取消界面等待不等于回滚外部副作用。所有用户操作都应通过最终刷新反映真实状态。

日志和镜像拉取响应使用有界消费：日志只请求固定尾部行数并限制单条/总展示长度；拉取完整消费响应流并检查其中的错误，不能把未消费的流留作后台泄漏。

## 状态与错误

- 快照保留最后一次成功数据，刷新失败不清空可用内容。
- `busy_operation` 阻止同一窗口提交冲突操作。
- Engine 错误转换为包含操作上下文的 `EngineError`，界面不直接展示 Debug 结构。
- 连接失败、权限不足、资源不存在和 Engine 返回冲突均应可见。
- 普通单元测试不依赖本机 Docker。

## 跨平台连接

首版调用 `Docker::connect_with_local_defaults()`，由 Bollard 处理：

| 平台 | 默认传输 |
| --- | --- |
| Linux/macOS | Unix socket |
| Windows | `//./pipe/docker_engine` named pipe |

`DOCKER_HOST` 可覆盖默认值。UI 不写死平台路径，也不把 Unix 路径作为 Windows 参数。后续远程连接必须单独处理 TLS、证书和凭据，不把开放 TCP 端口作为默认引导。

## 安全与数据保护

- Socket/named pipe 权限由操作系统和 Engine 管理；应用不主动提权。
- 容器删除默认 `v = false`、`force = false`；镜像删除不强制。
- 创建表单的 TCP 端口默认只绑定 `127.0.0.1`；创建后启动失败时，只 best-effort 清理本次返回的新容器 ID，不触碰已有资源。
- 启动命令使用 shell 风格引号解析为 Docker CMD 参数，不隐式经过 `/bin/sh` 执行。
- 停止和重启不伪装为可撤销操作，结果以 Engine 为准。
- 首版不采集遥测，不保存仓库凭据，不自动修改 Engine 配置。
- 日志可能包含秘密，只在本地展示；诊断导出功能实现前需增加脱敏提示。
- 集成测试必须显式启用，并限定 `rusker-test-*` 专用资源。

## 可测试性

纯函数测试覆盖格式化、过滤、日志截断和 revision 判定。Docker 集成测试作为独立、默认禁用的测试目标存在。UI 后续使用 GPUI test window 验证选择、确认框和异步状态；原生渲染、键盘导航与可访问性仍需 Windows/Linux/macOS 实机检查。

## 后续演进

WSL2/VM 管理应成为独立的 `runtime-host` 层，与 Docker API 客户端分离。其职责包括安装、启动、停止、资源限制、健康检查和恢复；不得把特权平台命令散落在 UI 回调中。
