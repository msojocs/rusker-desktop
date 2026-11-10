//! The Docker CLI's current context (`docker context use`), so plain
//! `docker` commands on Windows can be pointed at the managed engine.
//!
//! The CLI keeps the current context in `currentContext` of `config.json`
//! and each context in `contexts/meta/<sha256 of the name>/meta.json` next
//! to it. Both are written directly, like Docker Desktop does for
//! `desktop-linux`, so switching does not depend on `docker.exe` being on
//! `Path`.

use std::{
    fs,
    path::{Path, PathBuf},
};

use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

/// Name of the context that targets the managed engine.
pub const RUSKER_CONTEXT: &str = "rusker";
/// The CLI's built-in context; also what an unset `currentContext` means.
const DEFAULT_CONTEXT: &str = "default";
/// Key in the Rusker context's `Metadata` remembering the context that was
/// current before switching, for switching back. The CLI keeps unknown
/// metadata fields.
const PREVIOUS_KEY: &str = "RuskerPreviousContext";

/// Which engine plain `docker` commands of this user talk to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CliContext {
    /// `config.json` selects the Rusker context; switching back would
    /// select `previous`.
    Rusker { previous: String },
    /// Another context is current (`default` when none is set).
    Other(String),
    /// The named environment variable wins over `config.json`.
    Overridden(&'static str),
}

/// The CLI's current context according to the environment of this process
/// and the `config.json` at `config_path`.
pub fn current_context(config_path: &Path) -> Result<CliContext, String> {
    // Same precedence as the CLI: DOCKER_HOST, then DOCKER_CONTEXT, then
    // config.json.
    for name in ["DOCKER_HOST", "DOCKER_CONTEXT"] {
        if std::env::var_os(name).is_some_and(|value| !value.is_empty()) {
            return Ok(CliContext::Overridden(name));
        }
    }
    configured_context(config_path)
}

/// The context `config.json` selects, ignoring the environment.
fn configured_context(config_path: &Path) -> Result<CliContext, String> {
    let document = read_config(config_path)?;
    let current = configured_name(&document);
    Ok(if current == RUSKER_CONTEXT {
        CliContext::Rusker {
            previous: previous_context(config_path),
        }
    } else {
        CliContext::Other(current.to_string())
    })
}

/// Creates (or updates) the Rusker context with endpoint `host` and makes it
/// current, leaving every other key of `config.json` untouched. The context
/// that was current is remembered for [`restore_previous_context`].
pub fn use_rusker_context(config_path: &Path, host: &str) -> Result<(), String> {
    let mut document = read_config(config_path)?;
    let previous = match configured_name(&document) {
        // Already current: keep what the first switch remembered.
        RUSKER_CONTEXT => previous_context(config_path),
        current => current.to_string(),
    };
    let meta_path = context_meta_path(config_path, RUSKER_CONTEXT)?;
    let meta = json!({
        "Name": RUSKER_CONTEXT,
        "Metadata": { "Description": "Rusker Desktop", PREVIOUS_KEY: previous },
        "Endpoints": { "docker": { "Host": host, "SkipTLSVerify": false } },
    });
    write_json(&meta_path, &meta, &t!("docker_context.what.context"))?;

    if document.get("currentContext").and_then(Value::as_str) != Some(RUSKER_CONTEXT) {
        document.insert("currentContext".into(), RUSKER_CONTEXT.into());
        write_json(
            config_path,
            &Value::Object(document),
            &t!("docker_context.what.client_config"),
        )?;
    }
    Ok(())
}

/// Makes the context that was current before [`use_rusker_context`] current
/// again (`default` when unknown or since deleted) and returns its name. The
/// Rusker context itself is kept, like `docker context use` would.
pub fn restore_previous_context(config_path: &Path) -> Result<String, String> {
    let mut document = read_config(config_path)?;
    let previous = previous_context(config_path);
    if previous == DEFAULT_CONTEXT {
        // What `docker context use default` writes.
        document.remove("currentContext");
    } else {
        document.insert("currentContext".into(), previous.clone().into());
    }
    write_json(
        config_path,
        &Value::Object(document),
        &t!("docker_context.what.client_config"),
    )?;
    Ok(previous)
}

/// The context remembered in the Rusker context's metadata if it still
/// exists, otherwise `default`.
fn previous_context(config_path: &Path) -> String {
    let remembered = context_meta_path(config_path, RUSKER_CONTEXT)
        .ok()
        .and_then(|path| fs::read_to_string(path).ok())
        .and_then(|content| serde_json::from_str::<Value>(&content).ok())
        .and_then(|meta| {
            meta.get("Metadata")?
                .get(PREVIOUS_KEY)?
                .as_str()
                .map(str::to_string)
        })
        .filter(|name| !name.is_empty() && name != RUSKER_CONTEXT);
    match remembered {
        Some(name)
            if name == DEFAULT_CONTEXT
                || context_meta_path(config_path, &name).is_ok_and(|path| path.is_file()) =>
        {
            name
        }
        _ => DEFAULT_CONTEXT.to_string(),
    }
}

/// `contexts/meta/<sha256 hex of name>/meta.json` beside `config.json`.
fn context_meta_path(config_path: &Path, name: &str) -> Result<PathBuf, String> {
    let directory = config_path.parent().ok_or_else(|| {
        t!(
            "docker_context.error.no_parent_dir",
            path = config_path.display()
        )
        .into_owned()
    })?;
    let digest: String = Sha256::digest(name.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    Ok(directory
        .join("contexts")
        .join("meta")
        .join(digest)
        .join("meta.json"))
}

/// `currentContext` of `config.json`.
fn configured_name(document: &Map<String, Value>) -> &str {
    document
        .get("currentContext")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .unwrap_or(DEFAULT_CONTEXT)
}

/// `config.json` as an object; a missing or empty file is an empty object.
fn read_config(path: &Path) -> Result<Map<String, Value>, String> {
    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => {
            return Err(t!(
                "docker_context.error.read_failed",
                path = path.display(),
                error = error
            )
            .into_owned())
        }
    };
    if content.trim().is_empty() {
        return Ok(Map::new());
    }
    match serde_json::from_str::<Value>(&content) {
        Ok(Value::Object(map)) => Ok(map),
        Ok(_) => Err(t!(
            "docker_context.error.not_json_object",
            path = path.display()
        )
        .into_owned()),
        Err(error) => Err(t!(
            "docker_context.error.invalid_json",
            path = path.display(),
            error = error
        )
        .into_owned()),
    }
}

fn write_json(path: &Path, value: &Value, what: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            t!(
                "docker_context.error.create_dir_failed",
                what = what,
                path = parent.display(),
                error = error
            )
            .into_owned()
        })?;
    }
    let formatted = serde_json::to_string_pretty(value).map_err(|error| error.to_string())?;
    fs::write(path, format!("{formatted}\n")).map_err(|error| {
        t!(
            "docker_context.error.write_failed",
            what = what,
            path = path.display(),
            error = error
        )
        .into_owned()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOST: &str = "npipe:////./pipe/rusker_engine";

    fn read(path: &Path) -> Value {
        serde_json::from_str(&fs::read_to_string(path).expect("读取")).expect("JSON")
    }

    #[test]
    fn meta_directory_is_the_sha256_of_the_name() {
        let path = context_meta_path(Path::new("/home/u/.docker/config.json"), RUSKER_CONTEXT)
            .expect("路径");
        assert_eq!(
            path,
            Path::new("/home/u/.docker/contexts/meta")
                .join("d4bb99513a6252b8dfe2f7908754f009492c94e9bfdbd8de0be90bcbddbb39aa")
                .join("meta.json")
        );
    }

    #[test]
    fn switching_creates_the_context_and_keeps_other_keys() {
        let directory = tempfile::tempdir().expect("临时目录");
        let config = directory.path().join("config.json");
        fs::write(
            &config,
            r#"{"auths":{"ghcr.io":{}},"currentContext":"desktop-linux"}"#,
        )
        .expect("写入");
        assert_eq!(
            configured_context(&config).expect("读取"),
            CliContext::Other("desktop-linux".into())
        );

        use_rusker_context(&config, HOST).expect("切换");
        // desktop-linux has no meta.json here, so switching back would fall
        // back to default.
        assert_eq!(
            configured_context(&config).expect("读取"),
            CliContext::Rusker {
                previous: DEFAULT_CONTEXT.into()
            }
        );
        let document = read(&config);
        assert_eq!(document["currentContext"], RUSKER_CONTEXT);
        assert!(document["auths"]["ghcr.io"].is_object());
        let meta = read(&context_meta_path(&config, RUSKER_CONTEXT).expect("路径"));
        assert_eq!(meta["Name"], RUSKER_CONTEXT);
        assert_eq!(meta["Endpoints"]["docker"]["Host"], HOST);
    }

    #[test]
    fn switching_back_restores_the_previous_context() {
        let directory = tempfile::tempdir().expect("临时目录");
        let config = directory.path().join("config.json");
        let desktop = context_meta_path(&config, "desktop-linux").expect("路径");
        write_json(&desktop, &json!({ "Name": "desktop-linux" }), "测试").expect("写入");
        fs::write(&config, r#"{"currentContext":"desktop-linux"}"#).expect("写入");

        use_rusker_context(&config, HOST).expect("切换");
        // Switching again must not remember rusker itself.
        use_rusker_context(&config, HOST).expect("再次切换");
        assert_eq!(
            configured_context(&config).expect("读取"),
            CliContext::Rusker {
                previous: "desktop-linux".into()
            }
        );
        assert_eq!(
            restore_previous_context(&config).expect("切回"),
            "desktop-linux"
        );
        assert_eq!(
            configured_context(&config).expect("读取"),
            CliContext::Other("desktop-linux".into())
        );

        // Once that context is gone, switching back selects default by
        // removing currentContext.
        use_rusker_context(&config, HOST).expect("切换");
        fs::remove_file(&desktop).expect("删除");
        assert_eq!(restore_previous_context(&config).expect("切回"), "default");
        assert!(read(&config).get("currentContext").is_none());
    }

    #[test]
    fn missing_config_means_the_default_context() {
        let directory = tempfile::tempdir().expect("临时目录");
        let config = directory.path().join(".docker").join("config.json");
        assert_eq!(
            configured_context(&config).expect("读取"),
            CliContext::Other(DEFAULT_CONTEXT.into())
        );
        use_rusker_context(&config, HOST).expect("切换");
        assert_eq!(read(&config)["currentContext"], RUSKER_CONTEXT);
    }

    #[test]
    fn malformed_config_is_left_alone() {
        let directory = tempfile::tempdir().expect("临时目录");
        let config = directory.path().join("config.json");
        fs::write(&config, "{broken").expect("写入");
        assert!(configured_context(&config).is_err());
        assert!(use_rusker_context(&config, HOST).is_err());
        assert_eq!(fs::read_to_string(&config).expect("读取"), "{broken");
    }
}
