use std::{collections::HashSet, net::IpAddr};

use crate::model::{CreateContainerRequest, CreatePortBinding};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ContainerDraft {
    pub image: String,
    pub name: String,
    pub command: String,
    pub ports: String,
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
        };
        request.validate()?;
        Ok(request)
    }
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
