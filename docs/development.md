# 开发指南

## 工具链

- Rust 1.92 或更新版本（当前依赖图的最低要求）
- Cargo
- Git
- 本地开发建议使用 Docker Engine；普通单元测试不要求 Engine 运行

检查：

```bash
rustc --version
cargo --version
docker version
```

## Linux

GPUI Kit 官方在 Ubuntu 24.04 验证的基础依赖为：

```bash
sudo apt update
sudo apt install -y gcc g++ clang pkg-config libfontconfig-dev libwayland-dev \
  libwebkit2gtk-4.1-dev libxkbcommon-x11-dev libx11-xcb-dev \
  libssl-dev libzstd-dev libvulkan1
```

如启用 GPUI speech 功能，还需 `libasound2-dev`；当前项目未启用。运行窗口需要图形化 Wayland/X11 会话和可用的 Vulkan 驱动。其他发行版请安装对应开发包。

Linux socket 权限不足时，应正确配置用户组或 rootless socket，不建议用 `chmod 777 /var/run/docker.sock` 规避。

## Windows

- Windows 10 或更新版本
- Visual Studio 2022 Build Tools/Community，安装 **Desktop development with C++**、MSVC 和 Windows SDK
- CMake 加入 `PATH`
- 使用 Rust MSVC toolchain，而不是 GNU toolchain
- Linux musl 目标：`rustup target add x86_64-unknown-linux-musl`（ARM64 Windows 为 `aarch64-unknown-linux-musl`）

```powershell
rustup show active-toolchain
rustup target add x86_64-unknown-linux-musl
cmake --version
cargo run
```

`build.rs` 会把 `helpers/vm-proxy`（WSL2 VM 内的 Engine 代理转发程序，见[设置文档](settings.md#受管-engine与-docker-desktop-相同的代理结构)）构建为静态 Linux musl 程序并嵌入 exe，用 Rust 自带的 `rust-lld` 链接，不需要额外的交叉编译工具链。缺少目标时构建会报错并提示安装命令；也可设置 `RUSKER_VM_PROXY=<路径>` 嵌入预先构建好的程序。该辅助程序是独立的 Cargo workspace，单独检查：

```powershell
cargo fmt --manifest-path helpers/vm-proxy/Cargo.toml --check
$env:CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER='rust-lld'
cargo clippy --manifest-path helpers/vm-proxy/Cargo.toml --target x86_64-unknown-linux-musl -- -D warnings
```

未设置 `DOCKER_HOST` 时，Windows 版连接 Rusker 受管 WSL2 Engine 的 named pipe `//./pipe/rusker_engine`（由应用进程内的代理提供），不会使用 Docker Desktop 的 `//./pipe/docker_engine`。设置 `DOCKER_HOST` 可改连其他引擎。受管环境的安装、生命周期与集成见 [WSL2 运行环境文档](wsl2.md)。

WSL2 相关的实机测试默认跳过，需显式开启：

```powershell
# 安装/修复并启动专用发行版（会下载 rootfs 与软件包）
$env:RUSKER_WSL_RUNTIME_INTEGRATION='1'; cargo test --test runtime_host_operations
# 已安装时：自动启动、经 named pipe 访问、优雅关闭；可加 RUSKER_WSL_HOLD_SECS 保持运行
$env:RUSKER_WSL_RUNTIME_LIFECYCLE='1'; cargo test --test runtime_host_operations
# 在指定发行版启用并停用集成，并验证 docker run -i 的 stdin EOF
$env:RUSKER_WSL_INTEGRATION_DISTRO='Ubuntu-24.04'; cargo test --test runtime_host_operations
```

## macOS

- macOS 15 或更新版本
- Xcode Command Line Tools

```bash
xcode-select --install
cargo check
```

GPUI Kit 已启用 macOS `font-kit` 支持，不应另行混用不同版本的 GPUI 包。

## 网络代理

Cargo 和 curl 可使用标准代理环境变量，例如：

```bash
export HTTPS_PROXY="http://proxy.example.com:8080"
export HTTP_PROXY="$HTTPS_PROXY"
cargo fetch
```

不要把个人代理地址、凭据或内部镜像源提交到仓库。若使用 Cargo registry mirror，请在用户级 Cargo 配置中设置。`rustup target add` 同样遵循 `HTTPS_PROXY`。

## 日常检查

```bash
cargo fmt
cargo fmt --check
cargo check
cargo clippy --all-targets -- -D warnings
cargo test
```

首次构建会下载并编译较多 GPUI 原生依赖。开发 profile 只优化框架 crate，不改变业务代码的 Debug 属性，因此首次编译不会明显加快，但界面运行性能更好。

## 手动验收

1. 无 Engine 时启动应用，确认错误可读且界面仍可操作。
2. 启动 Engine 后重试，检查概览、全部容器和镜像。
3. 验证搜索无结果、清空搜索、选择不同容器和读取日志。
4. 仅使用 `rusker-test-*` 临时资源验证启动、停止、重启、删除和镜像拉取。
5. 连续点击刷新，确认旧响应不会覆盖新状态。
6. 在操作中尝试重复提交，确认按钮状态阻止冲突。
7. 检查删除确认框取消路径，以及默认不删除卷、不强制删除镜像。
8. 在 Windows 和 Linux 实机检查窗口缩放、输入、滚动、中文字体和长文本。
9. 使用临时 `RUSKER_CONFIG_DIR` 验证设置保存/重载、主题切换、非法代理 URL 和 Engine JSON 导出；测试不得改写真实 `daemon.json`。
10. 在支持托盘的桌面验证恢复、最小化、刷新、页面导航和退出。Linux 使用 `ksni` / StatusNotifierItem，不需要 GTK/AppIndicator；不支持托盘的桌面应降级为普通窗口模式。
11. Windows 上分别验证 named pipe 优先、显式 `DOCKER_HOST` 禁止回退、WSL2 默认/指定发行版，以及无 Linux CLI 或无权限时的错误提示。不要为测试自动安装或修改用户的发行版。

不要对用户已有容器或镜像执行破坏性测试。

## 可选 Docker 集成测试

集成测试默认跳过，只有显式设置 `RUSKER_DOCKER_INTEGRATION=1` 才运行。当前测试只读取真实 Engine 的版本、容器和镜像快照，不修改任何资源。CI 的普通 check/test 任务不需要 Docker daemon。

另有默认跳过的变更型测试。它要求先基于本地镜像创建一个专用标签，并只操作 `rusker-test-*` 资源：

```bash
docker tag alpine:3.23 rusker-test-engine:ci
RUSKER_DOCKER_OPERATIONS=1 cargo test --test engine_operations -- --nocapture
```

测试覆盖创建并启动、TCP 端口绑定、日志、启动、重启、停止、删除容器和删除专用镜像标签，并在结束时尝试清理。不要对任意已有资源运行变更型测试。

卷管理的变更型测试只创建和删除 `rusker-test-volume-*` 卷，首次运行可能拉取 `busybox:1.37`：

```bash
RUSKER_DOCKER_OPERATIONS=1 cargo test --test volume_operations -- --nocapture
```

它覆盖创建、重名拒绝、tar 导入、目录浏览、克隆、导出后再导入的往返一致性、清空、大小统计、单个与批量删除。Windows 上需把 `DOCKER_HOST` 指向被测 Engine（例如 `npipe:////./pipe/rusker_engine`）。

## 提交前

- 更新功能复选框，确保代码与文档一致。
- 不提交个人 socket、证书、凭据、代理或绝对路径。
- 不提交 `target/`、编辑器临时文件和生成的诊断日志。
- 新功能应有纯逻辑测试；平台相关变更至少执行对应平台的 `cargo check`。
- 未实机验证的平台或操作必须在交付说明中明确标记。

## 打包与发布

`.github/workflows/release.yml` 构建 Windows NSIS 安装包（`packaging/windows/installer.nsi`）和 Linux 的 `.deb` 与 `tar.gz`：

- 推送到 `master`：发布滚动预览版 `preview`（GitHub prerelease，每次覆盖），版本号为 `<Cargo 版本>-preview.<run 编号>`
- 推送 `v*` tag：发布正式 Release；tag 必须与 `Cargo.toml` 的 `version` 一致（如 `v0.1.0`），带 `-` 的版本（如 `v0.2.0-rc.1`）标记为 prerelease

```bash
# 发布正式版：先修改 Cargo.toml 中的 version 并提交
git tag v0.1.0
git push origin v0.1.0
```

Windows 安装包支持两种安装模式：

- 仅为我安装（默认）：安装到 `%LOCALAPPDATA%\Programs\Rusker Desktop`，写入 HKCU
- 为所有用户安装：安装到 `%ProgramFiles%\Rusker Desktop`，写入 HKLM，快捷方式放到公共开始菜单与桌面，需要管理员权限

安装包以普通权限启动，不弹 UAC。在“选择安装方式”页面选择“为所有用户安装”并点击“下一步”时才请求管理员权限：安装包会通过 UAC 以管理员身份重新启动并直接进入目录页，原进程退出；取消 UAC 则停留在该页。普通用户也可借助管理员凭据为所有用户安装。卸载为所有用户安装的版本时同样会请求管理员权限。静默安装可用 `/S /CurrentUser` 或 `/S /AllUsers`（后者会弹出 UAC）。升级时默认沿用已有安装的模式和目录。卸载不会删除用户设置和受管 WSL2 发行版（它们始终按用户存放）。
