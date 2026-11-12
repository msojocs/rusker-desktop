//! Plan bind mounts from a desktop container for the managed WSL engine.

use super::*;
use bollard::models::{Mount, MountType};
use std::collections::BTreeMap;
use tokio::process::Command;

const BIND_CHECK_TIMEOUT: Duration = Duration::from_secs(10);
const BIND_CHECK_SCRIPT: &str = r#"p=$1; [ -e "$p" ] && { [ -f "$p" ] || [ -d "$p" ]; }"#;

/// Translates source bind declarations into mounts usable by the managed WSL distro.
/// No filesystem changes are performed here.
pub(super) fn bind_mounts(inspect: &ContainerInspectResponse) -> Result<Vec<Mount>, EngineError> {
    let mut planned = BTreeMap::<String, Mount>::new();

    // MountPoints are resolved daemon metadata and are useful even when the
    // container was created but never started. Do not require them to exist.
    for point in inspect.mounts.as_deref().unwrap_or_default() {
        if point.typ.as_deref() != Some("bind") {
            continue;
        }
        let source = point.source.as_deref().unwrap_or_default();
        let target = point.destination.as_deref().unwrap_or_default();
        let mount = make_mount(
            source,
            target,
            !point.rw.unwrap_or(true),
            point.propagation.as_deref(),
        )?;
        insert_mount(&mut planned, mount)?;
    }

    if let Some(host) = inspect.host_config.as_ref() {
        for declaration in host.mounts.as_deref().unwrap_or_default() {
            match declaration.typ {
                Some(MountType::BIND) => {
                    if declaration.bind_options.as_ref().is_some_and(|options| {
                        options.non_recursive == Some(true)
                            || options.create_mountpoint == Some(true)
                            || options.read_only_non_recursive == Some(true)
                            || options.read_only_force_recursive == Some(true)
                    }) {
                        return Err(unsupported_options("bind option cannot be preserved"));
                    }
                    let propagation = declaration
                        .bind_options
                        .as_ref()
                        .and_then(|options| options.propagation.as_ref())
                        .map(|value| format!("{value:?}").to_ascii_lowercase());
                    let propagation = propagation.as_deref();
                    let mount = make_mount(
                        declaration.source.as_deref().unwrap_or_default(),
                        declaration.target.as_deref().unwrap_or_default(),
                        declaration.read_only.unwrap_or(false),
                        propagation,
                    )?;
                    insert_mount(&mut planned, mount)?;
                }
                Some(MountType::VOLUME | MountType::TMPFS) => {}
                _ => return Err(unsupported("unsupported host mount type")),
            }
        }
        for bind in host.binds.as_deref().unwrap_or_default() {
            let (source, target, read_only, propagation) = parse_bind(bind)?;
            if inspect
                .mounts
                .as_deref()
                .unwrap_or_default()
                .iter()
                .any(|mount| {
                    mount.typ.as_deref() == Some("volume") && mount.name.as_deref() == Some(source)
                })
            {
                continue;
            }
            let mount = make_mount(source, target, read_only, propagation)?;
            insert_mount(&mut planned, mount)?;
        }
    }

    Ok(planned.into_values().collect())
}

fn parse_bind(bind: &str) -> Result<(&str, &str, bool, Option<&str>), EngineError> {
    let bytes = bind.as_bytes();
    let offset =
        usize::from(bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':') * 2;
    let mut parts = bind[offset..].split(':');
    let first = parts.next().unwrap_or_default();
    let (source, target) = if offset == 2 {
        let source = &bind[..2 + first.len()];
        (source, parts.next().unwrap_or_default())
    } else {
        (first, parts.next().unwrap_or_default())
    };
    let mut read_only = false;
    let mut propagation = None;
    for option in parts {
        match option {
            "ro" => read_only = true,
            "rw" => read_only = false,
            "private" | "rprivate" | "shared" | "rshared" | "slave" | "rslave" => {
                if propagation.replace(option).is_some() {
                    return Err(unsupported_options("multiple bind propagation options"));
                }
            }
            "" => return Err(unsupported_options("empty bind option")),
            _ => return Err(unsupported_options("unknown bind option")),
        }
    }
    Ok((source, target, read_only, propagation))
}

fn make_mount(
    source: &str,
    target: &str,
    read_only: bool,
    propagation: Option<&str>,
) -> Result<Mount, EngineError> {
    if !target.starts_with('/') || target.starts_with("//") || has_parent_component(target) {
        return Err(unsupported("invalid bind destination"));
    }
    let source = normalize_source(source).ok_or_else(|| unsupported("unsupported bind source"))?;
    let bind_options = propagation
        .map(|propagation| {
            let propagation = propagation.to_ascii_lowercase();
            if !matches!(
                propagation.as_str(),
                "private" | "rprivate" | "shared" | "rshared" | "slave" | "rslave"
            ) {
                return Err(unsupported("unsupported bind propagation"));
            }
            Ok(serde_json::json!({ "Propagation": propagation }))
        })
        .transpose()?;
    let mut value = serde_json::json!({
        "Type": "bind",
        "Source": source,
        "Target": target,
        "ReadOnly": read_only
    });
    if let Some(options) = bind_options {
        value["BindOptions"] = options;
    }
    serde_json::from_value(value)
        .map_err(|error| unsupported(&format!("invalid bind options: {error}")))
}

fn normalize_source(source: &str) -> Option<String> {
    if source.is_empty() || source.chars().any(char::is_control) || has_parent_component(source) {
        return None;
    }
    if let Some(path) = crate::runtime_host::to_wsl_path(source) {
        return Some(path);
    }
    let normalized = source.replace('\\', "/");
    if normalized.starts_with("//") {
        return None;
    }
    let rest = normalized
        .strip_prefix("/run/desktop/mnt/host/")
        .or_else(|| normalized.strip_prefix("/mnt/"))?;
    let (drive, suffix) = rest.split_once('/').unwrap_or((rest, ""));
    if drive.len() != 1 || !drive.as_bytes()[0].is_ascii_alphabetic() {
        return None;
    }
    let mut result = format!("/mnt/{}", drive.to_ascii_lowercase());
    for component in suffix
        .split('/')
        .filter(|part| !part.is_empty() && *part != ".")
    {
        result.push('/');
        result.push_str(component);
    }
    Some(result)
}

fn has_parent_component(path: &str) -> bool {
    path.split(['/', '\\']).any(|part| part == "..")
}

fn insert_mount(planned: &mut BTreeMap<String, Mount>, mount: Mount) -> Result<(), EngineError> {
    let target = mount.target.clone().unwrap_or_default();
    if let Some(existing) = planned.get(&target) {
        if existing.source != mount.source || existing.read_only != mount.read_only {
            return Err(unsupported("conflicting mounts share a destination"));
        }
        if existing.bind_options.is_some()
            && mount.bind_options.is_some()
            && existing.bind_options != mount.bind_options
        {
            return Err(unsupported("conflicting bind propagation settings"));
        }
        if existing.bind_options.is_none() && mount.bind_options.is_some() {
            planned.insert(target, mount);
        }
        return Ok(());
    }
    planned.insert(target, mount);
    Ok(())
}

fn unsupported(detail: &str) -> EngineError {
    EngineError::operation(
        t!("engine.desktop.import_source_container"),
        t!("engine.desktop.bind_path_unsupported", detail = detail),
    )
}

fn unsupported_options(detail: &str) -> EngineError {
    EngineError::operation(
        t!("engine.desktop.import_source_container"),
        t!("engine.desktop.bind_options_unsupported", detail = detail),
    )
}

/// Checks that each source exists in the managed distro and is a regular file or directory.
/// The path is passed as a distinct argv value and is never interpolated into shell source.
pub(super) async fn check_bind_paths(mounts: &[Mount]) -> Result<(), EngineError> {
    #[cfg(windows)]
    {
        for mount in mounts {
            let path = mount.source.as_deref().unwrap_or_default();
            let mut command = Command::new("wsl.exe");
            command.args([
                "--distribution",
                crate::runtime_host::MANAGED_DISTRO_NAME,
                "--exec",
                "sh",
                "-c",
                BIND_CHECK_SCRIPT,
                "rusker-bind-check",
                path,
            ]);
            command.kill_on_drop(true);
            command.creation_flags(0x08000000);
            let output = tokio::time::timeout(BIND_CHECK_TIMEOUT, command.output())
                .await
                .map_err(|_| EngineError::Timeout {
                    action: t!("engine.desktop.bind_check").into_owned(),
                    seconds: BIND_CHECK_TIMEOUT.as_secs(),
                })?
                .map_err(|error| EngineError::operation(t!("engine.desktop.bind_check"), error))?;
            if !output.status.success() {
                return Err(EngineError::operation(
                    t!("engine.desktop.bind_check"),
                    t!("engine.desktop.bind_path_unavailable", path = path),
                ));
            }
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = mounts;
        Err(EngineError::operation(
            t!("engine.desktop.bind_check"),
            t!("runtime_host.error.unsupported_platform"),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn inspect(value: serde_json::Value) -> ContainerInspectResponse {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn plans_bind_from_mount_points_even_without_host_config() {
        let input = inspect(
            json!({"Mounts":[{"Type":"bind","Source":"C:\\用户 文件\\src","Destination":"/workspace","RW":false,"Propagation":"rprivate"}]}),
        );
        let mounts = bind_mounts(&input).unwrap();
        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0].source.as_deref(), Some("/mnt/c/用户 文件/src"));
        assert_eq!(mounts[0].read_only, Some(true));
        assert!(mounts[0].bind_options.is_some());
    }

    #[test]
    fn plans_unresolved_host_binds_and_deduplicates_destinations() {
        let input = inspect(
            json!({"HostConfig":{"Binds":["D:\\data:/data:ro","D:\\data:/data:ro","/mnt/e/project:/project:shared"]}}),
        );
        let mounts = bind_mounts(&input).unwrap();
        assert_eq!(mounts.len(), 2);
        assert_eq!(mounts[0].source.as_deref(), Some("/mnt/d/data"));
        assert_eq!(mounts[0].read_only, Some(true));
        assert_eq!(mounts[1].source.as_deref(), Some("/mnt/e/project"));
    }

    #[test]
    fn plans_host_mount_bind_options_and_resolved_duplicate_only_once() {
        let input = inspect(
            json!({"Mounts":[{"Type":"bind","Source":"/run/desktop/mnt/host/c/中文 path","Destination":"/data","RW":true}],"HostConfig":{"Mounts":[{"Type":"bind","Source":"C:\\中文 path","Target":"/data","ReadOnly":false,"BindOptions":{"Propagation":"rprivate"}}]}}),
        );
        let mounts = bind_mounts(&input).unwrap();
        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0].source.as_deref(), Some("/mnt/c/中文 path"));
    }

    #[test]
    fn rejects_unsupported_sources_options_and_path_escape() {
        for source in ["/var/lib/data", "\\\\server\\share", "C:\\..\\secret"] {
            assert!(normalize_source(source).is_none(), "{source}");
        }
        for declaration in ["C:\\data:/data:z", "C:\\data:/data:rshared:shared"] {
            assert!(parse_bind(declaration).is_err(), "{declaration}");
        }
        assert!(parse_bind("C:\\data:/data:ro").unwrap().2);
        assert_eq!(
            parse_bind("C:\\data:/data:ro:rshared").unwrap().3,
            Some("rshared")
        );
    }
}
