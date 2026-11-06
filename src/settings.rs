use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use thiserror::Error;
use url::Url;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemePreference {
    #[default]
    System,
    Light,
    Dark,
}

impl ThemePreference {
    pub fn label(self) -> &'static str {
        match self {
            Self::System => "跟随系统",
            Self::Light => "浅色",
            Self::Dark => "深色",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DockerProxySettings {
    pub enabled: bool,
    pub http_proxy: String,
    pub https_proxy: String,
    pub no_proxy: String,
}

impl DockerProxySettings {
    pub fn validate(&self) -> Result<(), String> {
        if !self.enabled {
            return Ok(());
        }
        if self.http_proxy.trim().is_empty() && self.https_proxy.trim().is_empty() {
            return Err("启用代理时，HTTP 或 HTTPS 代理至少填写一项".into());
        }
        validate_proxy_url("HTTP", &self.http_proxy)?;
        validate_proxy_url("HTTPS", &self.https_proxy)?;
        validate_no_proxy(&self.no_proxy)?;
        Ok(())
    }

    /// The `proxies` object for Docker's `daemon.json` (empty when disabled).
    pub fn engine_proxies(&self) -> Map<String, Value> {
        let mut proxies = Map::new();
        if self.enabled {
            insert_non_empty(&mut proxies, "http-proxy", &self.http_proxy);
            insert_non_empty(&mut proxies, "https-proxy", &self.https_proxy);
            insert_non_empty(&mut proxies, "no-proxy", &self.no_proxy);
        }
        proxies
    }

    pub fn engine_config_fragment(&self) -> String {
        let document = json!({ "proxies": Value::Object(self.engine_proxies()) });
        let formatted = serde_json::to_string_pretty(&document)
            .expect("proxy JSON serialization should be infallible");
        format!("{formatted}\n")
    }

    fn normalized(&self) -> Self {
        Self {
            enabled: self.enabled,
            http_proxy: self.http_proxy.trim().to_string(),
            https_proxy: self.https_proxy.trim().to_string(),
            no_proxy: self
                .no_proxy
                .split(',')
                .map(str::trim)
                .filter(|entry| !entry.is_empty())
                .collect::<Vec<_>>()
                .join(","),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WslSettings {
    pub integrations: BTreeMap<String, bool>,
}

impl WslSettings {
    fn normalized(&self) -> Self {
        Self {
            integrations: self
                .integrations
                .iter()
                .filter(|(name, enabled)| {
                    **enabled && !name.trim().is_empty() && name.trim() == name.as_str()
                })
                .map(|(name, enabled)| (name.clone(), *enabled))
                .collect(),
        }
    }
}

/// Shortest idle time before the managed engine enters Resource Saver mode.
pub const RESOURCE_SAVER_MIN_SECS: u64 = 30;
/// Longest idle time before the managed engine enters Resource Saver mode.
pub const RESOURCE_SAVER_MAX_SECS: u64 = 3600;
pub const RESOURCE_SAVER_DEFAULT_SECS: u64 = 300;

/// Docker Desktop-style Resource Saver: stop the managed engine after it has
/// had no running containers for `threshold_secs`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ResourceSaverSettings {
    pub enabled: bool,
    pub threshold_secs: u64,
}

impl Default for ResourceSaverSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            threshold_secs: RESOURCE_SAVER_DEFAULT_SECS,
        }
    }
}

impl ResourceSaverSettings {
    pub fn validate(&self) -> Result<(), String> {
        if !(RESOURCE_SAVER_MIN_SECS..=RESOURCE_SAVER_MAX_SECS).contains(&self.threshold_secs) {
            return Err(format!(
                "资源节省阈值需在 {RESOURCE_SAVER_MIN_SECS} 到 {RESOURCE_SAVER_MAX_SECS} 秒之间"
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct VmSettings {
    /// Directory holding the Docker data disk (`docker-data.vhdx`). Empty
    /// means the default location under the app data directory.
    pub disk_image_location: String,
}

impl VmSettings {
    pub fn validate(&self) -> Result<(), String> {
        let location = self.disk_image_location.trim();
        if !location.is_empty() && !Path::new(location).is_absolute() {
            return Err("磁盘镜像位置必须是绝对路径".into());
        }
        Ok(())
    }

    fn normalized(&self) -> Self {
        Self {
            disk_image_location: self.disk_image_location.trim().to_string(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppSettings {
    pub theme: ThemePreference,
    pub docker_proxy: DockerProxySettings,
    pub wsl: WslSettings,
    pub resource_saver: ResourceSaverSettings,
    pub vm: VmSettings,
}

impl AppSettings {
    pub fn validate(&self) -> Result<(), String> {
        self.docker_proxy.validate()?;
        self.resource_saver.validate()?;
        self.vm.validate()
    }

    pub fn normalized(&self) -> Self {
        Self {
            theme: self.theme,
            docker_proxy: self.docker_proxy.normalized(),
            wsl: self.wsl.normalized(),
            resource_saver: self.resource_saver.clone(),
            vm: self.vm.normalized(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct SettingsStore {
    root: PathBuf,
}

impl SettingsStore {
    pub fn discover() -> Result<Self, SettingsError> {
        if let Some(path) = std::env::var_os("RUSKER_CONFIG_DIR") {
            return Ok(Self::at(PathBuf::from(path)));
        }
        let directories = ProjectDirs::from("dev", "Rusker", "Rusker Desktop")
            .ok_or(SettingsError::DirectoryUnavailable)?;
        Ok(Self::at(directories.config_dir()))
    }

    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self { root: path.into() }
    }

    pub fn config_path(&self) -> PathBuf {
        self.root.join("settings.json")
    }

    pub fn proxy_export_path(&self) -> PathBuf {
        self.root.join("docker-proxies.json")
    }

    pub fn load(&self) -> Result<AppSettings, SettingsError> {
        let path = self.config_path();
        let content = match fs::read_to_string(&path) {
            Ok(content) => content,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(AppSettings::default());
            }
            Err(source) => {
                return Err(SettingsError::io("读取设置", path, source));
            }
        };
        let settings: AppSettings = serde_json::from_str(&content)
            .map_err(|source| SettingsError::Json { path, source })?;
        settings.validate().map_err(SettingsError::Validation)?;
        Ok(settings.normalized())
    }

    pub fn save(&self, settings: &AppSettings) -> Result<(), SettingsError> {
        let settings = settings.normalized();
        settings.validate().map_err(SettingsError::Validation)?;
        let path = self.config_path();
        self.write_json(&path, &settings)
    }

    pub fn export_proxy(&self, settings: &DockerProxySettings) -> Result<PathBuf, SettingsError> {
        let settings = settings.normalized();
        settings.validate().map_err(SettingsError::Validation)?;
        let path = self.proxy_export_path();
        self.write_text(&path, &settings.engine_config_fragment())?;
        Ok(path)
    }

    fn write_json<T: Serialize>(&self, path: &Path, value: &T) -> Result<(), SettingsError> {
        let content =
            serde_json::to_string_pretty(value).map_err(|source| SettingsError::Json {
                path: path.to_path_buf(),
                source,
            })?;
        self.write_text(path, &format!("{content}\n"))
    }

    fn write_text(&self, path: &Path, content: &str) -> Result<(), SettingsError> {
        fs::create_dir_all(&self.root)
            .map_err(|source| SettingsError::io("创建配置目录", &self.root, source))?;
        let mut options = OpenOptions::new();
        options.create(true).truncate(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(path)
            .map_err(|source| SettingsError::io("写入设置", path, source))?;
        file.write_all(content.as_bytes())
            .map_err(|source| SettingsError::io("写入设置", path, source))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600))
                .map_err(|source| SettingsError::io("设置文件权限", path, source))?;
        }
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum SettingsError {
    #[error("无法确定应用配置目录")]
    DirectoryUnavailable,
    #[error("{action}“{path}”失败：{source}")]
    Io {
        action: &'static str,
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("设置文件“{path}”格式无效：{source}")]
    Json {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("设置无效：{0}")]
    Validation(String),
}

impl SettingsError {
    fn io(action: &'static str, path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            action,
            path: path.into(),
            source,
        }
    }
}

fn insert_non_empty(map: &mut Map<String, Value>, key: &str, value: &str) {
    let value = value.trim();
    if !value.is_empty() {
        map.insert(key.into(), Value::String(value.into()));
    }
}

fn validate_proxy_url(label: &str, value: &str) -> Result<(), String> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(());
    }
    let url = Url::parse(value).map_err(|error| format!("{label} 代理地址无效：{error}"))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(format!(
            "{label} 代理仅支持带主机名的 http:// 或 https:// 地址"
        ));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(format!("{label} 代理不支持在地址中保存用户名或密码"));
    }
    if url.query().is_some()
        || url.fragment().is_some()
        || !url.path().is_empty() && url.path() != "/"
    {
        return Err(format!("{label} 代理地址不能包含路径、查询参数或片段"));
    }
    Ok(())
}

fn validate_no_proxy(value: &str) -> Result<(), String> {
    for entry in value
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
    {
        if entry.chars().any(char::is_whitespace) || entry.contains("://") {
            return Err(format!("NO_PROXY 条目“{entry}”无效"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const DISK_LOCATION: &str = if cfg!(windows) {
        "D:\\WSL\\Rusker"
    } else {
        "/srv/rusker"
    };

    #[test]
    fn resource_saver_defaults_and_threshold_bounds() {
        let saver = ResourceSaverSettings::default();
        assert!(saver.enabled);
        assert_eq!(saver.threshold_secs, RESOURCE_SAVER_DEFAULT_SECS);
        for (threshold, valid) in [(29, false), (30, true), (3600, true), (3601, false)] {
            let settings = ResourceSaverSettings {
                enabled: true,
                threshold_secs: threshold,
            };
            assert_eq!(settings.validate().is_ok(), valid, "阈值 {threshold}");
        }
        // Settings files written before Resource Saver existed keep loading.
        let legacy: AppSettings = serde_json::from_str(r#"{"theme":"dark"}"#).expect("旧设置");
        assert_eq!(legacy.resource_saver, ResourceSaverSettings::default());
        assert!(legacy.vm.disk_image_location.is_empty());
    }

    #[test]
    fn disk_image_location_must_be_absolute() {
        let relative = VmSettings {
            disk_image_location: "relative/dir".into(),
        };
        assert!(relative.validate().is_err());
        assert!(VmSettings::default().validate().is_ok());
    }

    fn proxy() -> DockerProxySettings {
        DockerProxySettings {
            enabled: true,
            http_proxy: " http://127.0.0.1:8080 ".into(),
            https_proxy: "https://proxy.example.com:8443".into(),
            no_proxy: "localhost, 127.0.0.1, ::1".into(),
        }
    }

    #[test]
    fn exports_engine_proxy_fragment_without_overwriting_other_daemon_keys() {
        let fragment = proxy().engine_config_fragment();
        let value: Value = serde_json::from_str(&fragment).expect("代理 JSON");
        assert_eq!(value["proxies"]["http-proxy"], "http://127.0.0.1:8080");
        assert_eq!(
            value["proxies"]["https-proxy"],
            "https://proxy.example.com:8443"
        );
        assert_eq!(value["proxies"]["no-proxy"], "localhost, 127.0.0.1, ::1");
        assert_eq!(value.as_object().expect("根对象").len(), 1);

        let disabled = DockerProxySettings::default().engine_config_fragment();
        let value: Value = serde_json::from_str(&disabled).expect("禁用代理 JSON");
        assert_eq!(value["proxies"], json!({}));
    }

    #[test]
    fn validates_proxy_urls_and_rejects_embedded_credentials() {
        assert!(proxy().validate().is_ok());
        for url in [
            "proxy.example.com:8080",
            "socks5://127.0.0.1:1080",
            "http://user:secret@127.0.0.1:8080",
            "http://127.0.0.1:8080/path",
        ] {
            let mut settings = proxy();
            settings.http_proxy = url.into();
            assert!(settings.validate().is_err(), "应拒绝 {url}");
        }
    }

    #[test]
    fn enabled_proxy_requires_at_least_one_address() {
        let settings = DockerProxySettings {
            enabled: true,
            ..DockerProxySettings::default()
        };
        assert!(settings.validate().is_err());
    }

    #[test]
    fn settings_round_trip_uses_user_supplied_directory() {
        let directory = tempfile::tempdir().expect("临时目录");
        let store = SettingsStore::at(directory.path());
        assert_eq!(store.load().expect("默认设置"), AppSettings::default());

        let settings = AppSettings {
            theme: ThemePreference::Dark,
            docker_proxy: proxy(),
            wsl: WslSettings {
                integrations: BTreeMap::from([
                    ("Ubuntu-24.04".into(), true),
                    ("Disabled".into(), false),
                ]),
            },
            resource_saver: ResourceSaverSettings {
                enabled: false,
                threshold_secs: 600,
            },
            vm: VmSettings {
                disk_image_location: format!(" {DISK_LOCATION} "),
            },
        };
        store.save(&settings).expect("保存设置");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(store.config_path())
                    .expect("设置文件元数据")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        let loaded = store.load().expect("重新读取设置");
        assert_eq!(loaded.theme, ThemePreference::Dark);
        assert_eq!(loaded.docker_proxy.http_proxy, "http://127.0.0.1:8080");
        assert_eq!(loaded.docker_proxy.no_proxy, "localhost,127.0.0.1,::1");
        assert_eq!(
            loaded.wsl.integrations,
            BTreeMap::from([("Ubuntu-24.04".into(), true)])
        );
        assert!(!loaded.resource_saver.enabled);
        assert_eq!(loaded.resource_saver.threshold_secs, 600);
        assert_eq!(loaded.vm.disk_image_location, DISK_LOCATION);

        let export = store.export_proxy(&loaded.docker_proxy).expect("导出代理");
        assert_eq!(export, directory.path().join("docker-proxies.json"));
        assert!(export.exists());
    }

    #[test]
    fn malformed_settings_return_an_error_instead_of_silent_data_loss() {
        let directory = tempfile::tempdir().expect("临时目录");
        let store = SettingsStore::at(directory.path());
        fs::create_dir_all(directory.path()).expect("配置目录");
        fs::write(store.config_path(), "{broken").expect("损坏设置");
        assert!(store.load().is_err());
    }
}
