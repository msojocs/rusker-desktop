use crate::model::{ContainerSummary, ImageSummary, PortMapping};
use time::{format_description::FormatItem, macros::format_description, OffsetDateTime};

const DATE_TIME_FORMAT: &[FormatItem<'static>] =
    format_description!("[year]-[month]-[day] [hour]:[minute]:[second] UTC");
const MAX_LOG_BYTES: usize = 256 * 1024;

pub fn short_id(id: &str) -> String {
    let id = id.strip_prefix("sha256:").unwrap_or(id);
    id.chars().take(12).collect()
}

pub fn format_bytes(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }

    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

pub fn format_timestamp(timestamp: i64) -> String {
    if timestamp <= 0 {
        return "—".into();
    }

    OffsetDateTime::from_unix_timestamp(timestamp)
        .ok()
        .and_then(|date| date.format(DATE_TIME_FORMAT).ok())
        .unwrap_or_else(|| timestamp.to_string())
}

pub fn format_ports(ports: &[PortMapping]) -> String {
    let mut mappings: Vec<String> = ports
        .iter()
        .map(|port| {
            let protocol = if port.protocol.is_empty() {
                "tcp"
            } else {
                port.protocol.as_str()
            };
            match port.public_port {
                Some(public) => {
                    let host = port.host_ip.as_deref().unwrap_or("0.0.0.0");
                    format!("{host}:{public}→{}/{protocol}", port.private_port)
                }
                None => format!("{}/{protocol}", port.private_port),
            }
        })
        .collect();
    mappings.sort();
    mappings.dedup();
    if mappings.is_empty() {
        "—".into()
    } else {
        mappings.join(", ")
    }
}

pub fn image_tags(image: &ImageSummary) -> String {
    let tags: Vec<&str> = image
        .tags
        .iter()
        .map(String::as_str)
        .filter(|tag| !tag.is_empty() && *tag != "<none>:<none>")
        .collect();
    if tags.is_empty() {
        "<none>".into()
    } else {
        tags.join(", ")
    }
}

pub fn container_matches(container: &ContainerSummary, query: &str) -> bool {
    let haystack = format!(
        "{} {} {} {} {}",
        container.name, container.id, container.image, container.state, container.status
    )
    .to_lowercase();
    query_matches(&haystack, query)
}

pub fn image_matches(image: &ImageSummary, query: &str) -> bool {
    let haystack = format!("{} {}", image.id, image.tags.join(" ")).to_lowercase();
    query_matches(&haystack, query)
}

fn query_matches(haystack: &str, query: &str) -> bool {
    query
        .split_whitespace()
        .all(|term| haystack.contains(&term.to_lowercase()))
}

pub fn bound_logs(logs: &str) -> String {
    let logs = logs.replace('\0', "�");
    if logs.len() <= MAX_LOG_BYTES {
        return logs;
    }

    let mut start = logs.len() - MAX_LOG_BYTES;
    while !logs.is_char_boundary(start) {
        start += 1;
    }
    format!("… 较早日志已截断 …\n{}", &logs[start..])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn container() -> ContainerSummary {
        ContainerSummary {
            id: "abcdef1234567890".into(),
            name: "web-1".into(),
            image: "docker.io/library/nginx:latest".into(),
            state: "running".into(),
            status: "Up 2 hours".into(),
            ..ContainerSummary::default()
        }
    }

    #[test]
    fn short_id_removes_digest_prefix_and_limits_length() {
        assert_eq!(short_id("sha256:abcdef1234567890"), "abcdef123456");
        assert_eq!(short_id("abc"), "abc");
        assert_eq!(short_id(""), "");
    }

    #[test]
    fn formats_binary_sizes() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1023), "1023 B");
        assert_eq!(format_bytes(1024), "1.0 KiB");
        assert_eq!(format_bytes(5 * 1024 * 1024), "5.0 MiB");
    }

    #[test]
    fn formats_ports_without_duplicates() {
        let ports = vec![
            PortMapping {
                host_ip: Some("127.0.0.1".into()),
                private_port: 80,
                public_port: Some(8080),
                protocol: "tcp".into(),
            },
            PortMapping {
                host_ip: Some("127.0.0.1".into()),
                private_port: 80,
                public_port: Some(8080),
                protocol: "tcp".into(),
            },
            PortMapping {
                private_port: 53,
                protocol: "udp".into(),
                ..PortMapping::default()
            },
        ];
        assert_eq!(format_ports(&ports), "127.0.0.1:8080→80/tcp, 53/udp");
    }

    #[test]
    fn container_filter_matches_all_terms_case_insensitively() {
        assert!(container_matches(&container(), "WEB nginx"));
        assert!(container_matches(&container(), "abcdef"));
        assert!(!container_matches(&container(), "postgres"));
    }

    #[test]
    fn empty_filter_matches_everything() {
        assert!(container_matches(&container(), "  "));
    }

    #[test]
    fn logs_are_bounded_without_splitting_utf8() {
        let logs = "测".repeat(MAX_LOG_BYTES);
        let bounded = bound_logs(&logs);
        assert!(bounded.starts_with("… 较早日志已截断 …\n"));
        assert!(bounded.len() <= MAX_LOG_BYTES + 64);
        assert_eq!(bound_logs("a\0b"), "a�b");
    }

    #[test]
    fn invalid_timestamps_fall_back_cleanly() {
        assert_eq!(format_timestamp(0), "—");
        assert_eq!(format_timestamp(-1), "—");
        assert_eq!(format_timestamp(i64::MAX), i64::MAX.to_string());
    }
}
