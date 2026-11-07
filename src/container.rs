use std::{collections::HashSet, net::IpAddr};

use crate::model::{CreateContainerRequest, CreatePortBinding};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ContainerDraft {
    pub image: String,
    pub name: String,
    pub command: String,
    pub ports: String,
    pub nvidia_gpu: bool,
    /// `(host path or volume name, container path)` rows; blank rows are ignored.
    pub mounts: Vec<(String, String)>,
    /// `(variable, value)` rows; rows without a variable name are ignored
    /// unless they carry a value.
    pub env: Vec<(String, String)>,
}

impl ContainerDraft {
    pub fn parse(&self) -> Result<CreateContainerRequest, String> {
        let command = shell_words::split(self.command.trim())
            .map_err(|error| format!("启动命令解析失败：{error}"))?;
        let request = CreateContainerRequest {
            image: self.image.trim().to_string(),
            name: optional_text(&self.name),
            command,
            ports: parse_ports(&self.ports)?,
            nvidia_gpu: self.nvidia_gpu,
            env: parse_env(&self.env)?,
            binds: parse_mounts(&self.mounts)?,
        };
        request.validate()?;
        Ok(request)
    }
}

/// Adds `defaults` (`NAME=value`) for every variable `env` does not set.
pub fn merge_env(env: &mut Vec<String>, defaults: Vec<String>) {
    let keys: HashSet<String> = env
        .iter()
        .map(|entry| entry.split('=').next().unwrap_or_default().to_string())
        .collect();
    let mut merged: Vec<String> = defaults
        .into_iter()
        .filter(|entry| !keys.contains(entry.split('=').next().unwrap_or_default()))
        .collect();
    merged.append(env);
    *env = merged;
}

fn parse_env(rows: &[(String, String)]) -> Result<Vec<String>, String> {
    let mut seen = HashSet::new();
    let mut env = Vec::new();
    for (name, value) in rows {
        let name = name.trim();
        if name.is_empty() {
            if value.trim().is_empty() {
                continue;
            }
            return Err(format!("环境变量值“{value}”缺少变量名"));
        }
        if name.contains('=') || name.chars().any(char::is_whitespace) {
            return Err(format!("环境变量名“{name}”不能包含等号或空白字符"));
        }
        if !seen.insert(name.to_string()) {
            return Err(format!("环境变量 {name} 重复"));
        }
        env.push(format!("{name}={value}"));
    }
    Ok(env)
}

fn parse_mounts(rows: &[(String, String)]) -> Result<Vec<String>, String> {
    let mut targets = HashSet::new();
    let mut binds = Vec::new();
    for (source, target) in rows {
        let (source, target) = (source.trim(), target.trim());
        match (source.is_empty(), target.is_empty()) {
            (true, true) => continue,
            (true, false) => return Err(format!("挂载到 {target} 的宿主机路径不能为空")),
            (false, true) => return Err(format!("宿主机路径 {source} 缺少容器路径")),
            _ => {}
        }
        if !target.starts_with('/') {
            return Err(format!("容器路径“{target}”必须是以 / 开头的绝对路径"));
        }
        if target.contains(':') {
            return Err(format!("容器路径“{target}”不能包含冒号"));
        }
        if !targets.insert(target.to_string()) {
            return Err(format!("容器路径 {target} 被重复挂载"));
        }
        binds.push(format!("{source}:{target}"));
    }
    Ok(binds)
}

impl CreateContainerRequest {
    pub fn validate(&self) -> Result<(), String> {
        if self.image.is_empty() {
            return Err("镜像引用不能为空".into());
        }
        if self.image.chars().any(char::is_whitespace) {
            return Err("镜像引用不能包含空白字符".into());
        }
        if let Some(name) = &self.name {
            validate_name(name)?;
        }

        let mut host_bindings = HashSet::new();
        for port in &self.ports {
            if port.host_port == 0 || port.container_port == 0 {
                return Err("端口必须大于 0".into());
            }
            if port.host_ip.parse::<IpAddr>().is_err() {
                return Err(format!("无效的宿主机 IP 地址：{}", port.host_ip));
            }
            if !host_bindings.insert((&port.host_ip, port.host_port)) {
                return Err(format!(
                    "宿主机端口 {}:{} 被重复绑定",
                    port.host_ip, port.host_port
                ));
            }
        }
        Ok(())
    }
}

fn optional_text(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

fn validate_name(name: &str) -> Result<(), String> {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return Err("容器名称不能为空".into());
    };
    if !first.is_ascii_alphanumeric()
        || !chars.all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '_' | '.' | '-')
        })
    {
        return Err(
            "容器名称只能包含字母、数字、下划线、点和连字符，且必须以字母或数字开头".into(),
        );
    }
    if name.len() < 2 {
        return Err("容器名称至少需要 2 个字符".into());
    }
    Ok(())
}

fn parse_ports(value: &str) -> Result<Vec<CreatePortBinding>, String> {
    if value.trim().is_empty() {
        return Ok(Vec::new());
    }

    value
        .split(',')
        .map(|entry| {
            let entry = entry.trim();
            let parts: Vec<&str> = entry.split(':').collect();
            let [host, container] = parts.as_slice() else {
                return Err(format!("端口映射“{entry}”无效，应为 宿主机端口:容器端口"));
            };
            let host_port = parse_port(host)?;
            let container_port = parse_port(container)?;
            Ok(CreatePortBinding {
                host_ip: "127.0.0.1".into(),
                host_port,
                container_port,
            })
        })
        .collect()
}

fn parse_port(value: &str) -> Result<u16, String> {
    let port = value
        .trim()
        .parse::<u16>()
        .map_err(|_| format!("端口“{value}”无效，应为 1–65535 的整数"))?;
    if port == 0 {
        return Err("端口必须大于 0".into());
    }
    Ok(port)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draft() -> ContainerDraft {
        ContainerDraft {
            image: " alpine:3.23 ".into(),
            ..ContainerDraft::default()
        }
    }

    #[test]
    fn parses_minimal_draft_with_engine_defaults() {
        let request = draft().parse().expect("最小配置");
        assert_eq!(request.image, "alpine:3.23");
        assert_eq!(request.name, None);
        assert!(request.command.is_empty());
        assert!(request.ports.is_empty());
        assert!(!request.nvidia_gpu);
    }

    #[test]
    fn preserves_explicit_nvidia_gpu_request() {
        let request = ContainerDraft {
            nvidia_gpu: true,
            ..draft()
        }
        .parse()
        .expect("GPU 配置");
        assert!(request.nvidia_gpu);
    }

    #[test]
    fn parses_quoted_command_without_invoking_a_shell() {
        let request = ContainerDraft {
            command: "sh -c \"echo hello world; sleep 5\"".into(),
            ..draft()
        }
        .parse()
        .expect("带引号命令");
        assert_eq!(request.command, ["sh", "-c", "echo hello world; sleep 5"]);
    }

    #[test]
    fn reports_unclosed_command_quote() {
        let error = ContainerDraft {
            command: "sh -c \"echo broken".into(),
            ..draft()
        }
        .parse()
        .expect_err("未闭合引号");
        assert!(error.contains("启动命令解析失败"));
    }

    #[test]
    fn validates_container_names() {
        for name in ["web-1", "web_api", "web.api", "a1"] {
            let request = ContainerDraft {
                name: name.into(),
                ..draft()
            }
            .parse()
            .expect("合法名称");
            assert_eq!(request.name.as_deref(), Some(name));
        }
        for name in ["-web", "web api", "a", "容器"] {
            assert!(ContainerDraft {
                name: name.into(),
                ..draft()
            }
            .parse()
            .is_err());
        }
    }

    #[test]
    fn parses_multiple_tcp_ports_on_loopback() {
        let request = ContainerDraft {
            ports: "8080:80, 8443:443".into(),
            ..draft()
        }
        .parse()
        .expect("合法端口");
        assert_eq!(request.ports.len(), 2);
        assert_eq!(request.ports[0].host_ip, "127.0.0.1");
        assert_eq!(request.ports[0].host_port, 8080);
        assert_eq!(request.ports[0].container_port, 80);
        assert_eq!(request.ports[1].container_port, 443);
    }

    #[test]
    fn rejects_invalid_port_syntax_and_ranges() {
        for ports in ["8080", "a:80", "0:80", "8080:0", "8080:65536", "8080:80,"] {
            assert!(ContainerDraft {
                ports: ports.into(),
                ..draft()
            }
            .parse()
            .is_err());
        }
    }

    #[test]
    fn rejects_duplicate_host_bindings_even_for_different_container_ports() {
        let error = ContainerDraft {
            ports: "8080:80,8080:443".into(),
            ..draft()
        }
        .parse()
        .expect_err("重复宿主机端口");
        assert!(error.contains("重复绑定"));
    }

    #[test]
    fn parses_mounts_and_skips_blank_rows() {
        let request = ContainerDraft {
            mounts: vec![
                (r"C:\src".into(), "/src".into()),
                ("".into(), " ".into()),
                ("data".into(), "/var/lib/data".into()),
            ],
            ..draft()
        }
        .parse()
        .expect("合法挂载");
        assert_eq!(request.binds, [r"C:\src:/src", "data:/var/lib/data"]);
    }

    #[test]
    fn rejects_incomplete_relative_or_duplicate_mounts() {
        for mounts in [
            vec![("/a".to_string(), "".to_string())],
            vec![("".into(), "/a".into())],
            vec![("/a".into(), "a".into())],
            vec![("/a".into(), "/x".into()), ("/b".into(), "/x".into())],
        ] {
            assert!(ContainerDraft { mounts, ..draft() }.parse().is_err());
        }
    }

    #[test]
    fn parses_env_rows() {
        let request = ContainerDraft {
            env: vec![
                ("TZ".into(), "Asia/Shanghai".into()),
                ("".into(), "".into()),
                ("EMPTY".into(), "".into()),
                ("EQ".into(), "a=b".into()),
            ],
            ..draft()
        }
        .parse()
        .expect("合法环境变量");
        assert_eq!(request.env, ["TZ=Asia/Shanghai", "EMPTY=", "EQ=a=b"]);
        for env in [
            vec![("".to_string(), "x".to_string())],
            vec![("A=B".into(), "x".into())],
            vec![("A".into(), "1".into()), ("A".into(), "2".into())],
        ] {
            assert!(ContainerDraft { env, ..draft() }.parse().is_err());
        }
    }

    #[test]
    fn user_env_overrides_defaults() {
        let mut env = vec!["HTTP_PROXY=http://user".to_string()];
        merge_env(
            &mut env,
            vec!["HTTP_PROXY=http://default".into(), "NO_PROXY=local".into()],
        );
        assert_eq!(env, ["NO_PROXY=local", "HTTP_PROXY=http://user"]);
    }

    #[test]
    fn rejects_blank_or_whitespace_image_references() {
        assert!(draft().parse().is_ok());
        assert!(ContainerDraft::default().parse().is_err());
        assert!(ContainerDraft {
            image: "alpine latest".into(),
            ..ContainerDraft::default()
        }
        .parse()
        .is_err());
    }
}
