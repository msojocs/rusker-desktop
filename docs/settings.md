# 设置与 Docker Engine 代理

## 设置界面

侧边栏“设置”目前提供：

- 外观：跟随系统、浅色、深色，选择后立即生效，保存后持久化。
- Docker Engine 拉取代理：开关、`HTTP_PROXY`、`HTTPS_PROXY`、`NO_PROXY`。
- 保存 Rusker 设置。
- 导出 Docker `daemon.json` 使用的 `proxies` JSON 片段。

默认配置目录由操作系统决定，可通过 `RUSKER_CONFIG_DIR` 覆盖。目录内包含：

- `settings.json`：Rusker 自身设置。
- `docker-proxies.json`：导出的 Engine 代理片段。

Unix 下新写入的设置文件权限为 `0600`。代理地址不支持内嵌用户名或密码，避免在明文凭据配置中保存秘密。

## 保存与生效的区别

**保存 Rusker 设置不会自动修改 Docker 配置，也不会重启 Engine。** “导出 Engine JSON”同样只写入 Rusker 配置目录。

要让代理影响镜像拉取，需要手动完成以下步骤：

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

## Windows

独立 Docker Engine 常见配置路径为 `C:\ProgramData\Docker\config\daemon.json`。Docker Desktop、Rancher Desktop 或其他兼容 Engine 可能使用各自的配置界面和虚拟机网络，不应直接假设该路径有效。

请通过对应产品的受支持方式合并代理并重启 Engine。Rusker 当前不会尝试修改 Windows 服务、WSL2 或注册表。

## 代理字段

- `HTTP_PROXY` / `HTTPS_PROXY`：仅支持带主机名的 `http://`、`https://` 地址，不允许路径、查询参数、片段或内嵌凭据。
- `NO_PROXY`：逗号分隔的主机名、域名、IP 或 CIDR，例如 `localhost,127.0.0.1,::1,.internal.example.com`。
- 启用代理时，HTTP 和 HTTPS 地址至少填写一个。

这些设置面向 **Docker Engine 的出站请求**，不是 Rusker 应用代理，也不会自动注入已有或新建容器的环境变量。
