use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::Write,
    net::Ipv4Addr,
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
    pub fn label(self) -> String {
        match self {
            Self::System => t!("settings.theme.system"),
            Self::Light => t!("settings.theme.light"),
            Self::Dark => t!("settings.theme.dark"),
        }
        .into_owned()
    }
}

/// Where a proxy setting takes its addresses from (Docker Desktop's
/// proxy drop-down).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProxyMode {
    /// Containers only: whatever the Engine (host) proxy resolves to.
    SameAsHost,
    /// The operating system's proxy, read when it is applied.
    System,
    #[default]
    None,
    Manual,
}

impl ProxyMode {
    pub const ENGINE: [Self; 3] = [Self::System, Self::None, Self::Manual];
    pub const CONTAINER: [Self; 4] = [Self::SameAsHost, Self::System, Self::None, Self::Manual];

    pub fn label(self) -> String {
        match self {
            Self::SameAsHost => t!("settings.proxy_mode.same_as_host"),
            Self::System => t!("settings.proxy_mode.system"),
            Self::None => t!("settings.proxy_mode.none"),
            Self::Manual => t!("settings.proxy_mode.manual"),
        }
        .into_owned()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "StoredProxySettings")]
pub struct DockerProxySettings {
    pub mode: ProxyMode,
    /// Manual configuration; kept while another mode is selected.
    pub http_proxy: String,
    pub https_proxy: String,
    pub no_proxy: String,
}

/// On-disk form; settings written before proxy modes existed only have
/// `enabled` (true meant a manual configuration).
#[derive(Default, Deserialize)]
#[serde(default)]
struct StoredProxySettings {
    mode: Option<ProxyMode>,
    enabled: Option<bool>,
    http_proxy: String,
    https_proxy: String,
    no_proxy: String,
}

impl From<StoredProxySettings> for DockerProxySettings {
    fn from(stored: StoredProxySettings) -> Self {
        let mode = stored.mode.unwrap_or(match stored.enabled {
            Some(true) => ProxyMode::Manual,
            _ => ProxyMode::None,
        });
        Self {
            mode,
            http_proxy: stored.http_proxy,
            https_proxy: stored.https_proxy,
            no_proxy: stored.no_proxy,
        }
    }
}

/// Proxy addresses after resolving the mode (all empty: no proxy).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ResolvedProxy {
    pub http_proxy: String,
    pub https_proxy: String,
    pub no_proxy: String,
}

impl ResolvedProxy {
    pub fn is_empty(&self) -> bool {
        self.http_proxy.is_empty() && self.https_proxy.is_empty()
    }

    fn fields(&self) -> [(&'static str, &str); 3] {
        [
            ("HTTP_PROXY", self.http_proxy.as_str()),
            ("HTTPS_PROXY", self.https_proxy.as_str()),
            ("NO_PROXY", self.no_proxy.as_str()),
        ]
    }

    fn to_map(&self, keys: [&str; 3]) -> Map<String, Value> {
        let mut map = Map::new();
        if !self.is_empty() {
            for (key, (_, value)) in keys.into_iter().zip(self.fields()) {
                insert_non_empty(&mut map, key, value);
            }
        }
        map
    }

    /// The `proxies` object for Docker's `daemon.json`.
    pub fn engine_proxies(&self) -> Map<String, Value> {
        self.to_map(["http-proxy", "https-proxy", "no-proxy"])
    }

    /// The `proxies.default` object of the Docker CLI's `config.json`. The
    /// CLI injects it into containers it creates and into builds as
    /// `HTTP_PROXY`/`HTTPS_PROXY`/`NO_PROXY`.
    pub fn client_proxies(&self) -> Map<String, Value> {
        self.to_map(["httpProxy", "httpsProxy", "noProxy"])
    }

    /// Environment variables for a new container, upper- and lowercase
    /// like the Docker CLI sets them.
    pub fn container_env(&self) -> Vec<String> {
        if self.is_empty() {
            return Vec::new();
        }
        let mut env = Vec::new();
        for (name, value) in self.fields() {
            if !value.is_empty() {
                env.push(format!("{name}={value}"));
                env.push(format!("{}={value}", name.to_ascii_lowercase()));
            }
        }
        env
    }
}

impl DockerProxySettings {
    pub fn validate(&self) -> Result<(), String> {
        if self.mode != ProxyMode::Manual {
            return Ok(());
        }
        if self.http_proxy.trim().is_empty() && self.https_proxy.trim().is_empty() {
            return Err(t!("settings.error.manual_proxy_required").into_owned());
        }
        validate_proxy_url("HTTP", &self.http_proxy)?;
        validate_proxy_url("HTTPS", &self.https_proxy)?;
        validate_no_proxy(&self.no_proxy)?;
        Ok(())
    }

    /// The addresses this setting stands for right now. `host` is what
    /// "same as host proxy" refers to; the system proxy is read on each call.
    pub fn resolve(&self, host: Option<&DockerProxySettings>) -> ResolvedProxy {
        match self.mode {
            ProxyMode::None => ResolvedProxy::default(),
            ProxyMode::Manual => {
                let manual = self.normalized();
                ResolvedProxy {
                    http_proxy: manual.http_proxy,
                    https_proxy: manual.https_proxy,
                    no_proxy: manual.no_proxy,
                }
            }
            ProxyMode::System => system_proxy(),
            ProxyMode::SameAsHost => host
                .filter(|host| host.mode != ProxyMode::SameAsHost)
                .map(|host| host.resolve(None))
                .unwrap_or_default(),
        }
    }

    /// The Engine's `proxies` object for `daemon.json` (empty for no proxy).
    pub fn engine_proxies(&self) -> Map<String, Value> {
        self.resolve(None).engine_proxies()
    }

    pub fn engine_config_fragment(&self) -> String {
        let document = json!({ "proxies": Value::Object(self.engine_proxies()) });
        let formatted = serde_json::to_string_pretty(&document)
            .expect("proxy JSON serialization should be infallible");
        format!("{formatted}\n")
    }

    fn normalized(&self) -> Self {
        Self {
            mode: self.mode,
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
            return Err(t!(
                "settings.error.resource_saver_threshold",
                min = RESOURCE_SAVER_MIN_SECS,
                max = RESOURCE_SAVER_MAX_SECS
            )
            .into_owned());
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
            return Err(t!("settings.error.disk_location_not_absolute").into_owned());
        }
        Ok(())
    }

    fn normalized(&self) -> Self {
        Self {
            disk_image_location: self.disk_image_location.trim().to_string(),
        }
    }
}

/// Which host addresses published container ports listen on (Docker
/// Desktop's "Port binding behavior").
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PortBinding {
    /// Docker's default: every interface (`0.0.0.0` / `::`).
    #[default]
    Open,
    /// Only `127.0.0.1`, unless a port mapping names another address.
    LocalhostOnly,
}

impl PortBinding {
    pub const ALL: [Self; 2] = [Self::Open, Self::LocalhostOnly];

    pub fn label(self) -> String {
        match self {
            Self::Open => t!("settings.port_binding.open"),
            Self::LocalhostOnly => t!("settings.port_binding.localhost_only"),
        }
        .into_owned()
    }
}

/// IP families of new networks (Docker Desktop's "Default networking mode").
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NetworkingMode {
    #[default]
    Ipv4Only,
    /// IPv4 and IPv6 on the default bridge and new bridge networks.
    DualStack,
}

impl NetworkingMode {
    pub const ALL: [Self; 2] = [Self::Ipv4Only, Self::DualStack];

    pub fn label(self) -> String {
        match self {
            Self::Ipv4Only => t!("settings.networking_mode.ipv4_only"),
            Self::DualStack => t!("settings.networking_mode.dual_stack"),
        }
        .into_owned()
    }
}

/// Docker's default bridge network when no subnet is set.
pub const DEFAULT_DOCKER_SUBNET: &str = "172.17.0.0/16";
/// IPv6 subnet of the default bridge in dual-stack mode (a ULA prefix).
pub const DEFAULT_BRIDGE_IPV6_SUBNET: &str = "fd00:7275:736b::/64";

/// `daemon.json` keys owned by [`NetworkSettings`].
pub const ENGINE_NETWORK_KEYS: [&str; 5] =
    ["bip", "ip", "ipv6", "fixed-cidr-v6", "default-network-opts"];

/// Docker Desktop-style network settings, applied to the managed Engine's
/// `daemon.json`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct NetworkSettings {
    /// IPv4 subnet of the default bridge (`docker0`) in CIDR notation.
    /// Empty: Docker's default ([`DEFAULT_DOCKER_SUBNET`]).
    pub subnet: String,
    /// Docker Desktop's "Enable host networking": also forward ports that
    /// `--net=host` containers listen on (NAT LAN forwarding, UDP on
    /// localhost). Takes effect immediately; not a `daemon.json` key.
    pub host_networking: bool,
    pub port_binding: PortBinding,
    pub mode: NetworkingMode,
}

impl NetworkSettings {
    pub fn validate(&self) -> Result<(), String> {
        let subnet = self.subnet.trim();
        if !subnet.is_empty() {
            parse_ipv4_subnet(subnet)?;
        }
        Ok(())
    }

    fn normalized(&self) -> Self {
        Self {
            subnet: self.subnet.trim().to_string(),
            ..self.clone()
        }
    }

    /// The [`ENGINE_NETWORK_KEYS`] this configuration sets; the others are
    /// left to Docker's defaults. An invalid subnet contributes nothing.
    pub fn engine_config(&self) -> Map<String, Value> {
        let mut config = Map::new();
        if let Ok((network, prefix)) = parse_ipv4_subnet(self.subnet.trim()) {
            // `bip` is the bridge's own address: the subnet's first host.
            let gateway = Ipv4Addr::from(u32::from(network) + 1);
            config.insert("bip".into(), Value::String(format!("{gateway}/{prefix}")));
        }
        if self.port_binding == PortBinding::LocalhostOnly {
            config.insert("ip".into(), Value::String("127.0.0.1".into()));
        }
        if self.mode == NetworkingMode::DualStack {
            config.insert("ipv6".into(), Value::Bool(true));
            config.insert(
                "fixed-cidr-v6".into(),
                Value::String(DEFAULT_BRIDGE_IPV6_SUBNET.into()),
            );
            config.insert(
                "default-network-opts".into(),
                json!({ "bridge": { "com.docker.network.enable_ipv6": "true" } }),
            );
        }
        config
    }
}

/// Parses an IPv4 network such as `172.17.0.0/16`, rejecting host bits and
/// prefixes too long for a bridge with containers.
fn parse_ipv4_subnet(value: &str) -> Result<(Ipv4Addr, u8), String> {
    let invalid = || {
        t!(
            "settings.error.subnet_invalid",
            value = value,
            example = DEFAULT_DOCKER_SUBNET
        )
        .into_owned()
    };
    let (address, prefix) = value.split_once('/').ok_or_else(invalid)?;
    let address: Ipv4Addr = address.trim().parse().map_err(|_| invalid())?;
    let prefix: u8 = prefix.trim().parse().map_err(|_| invalid())?;
    if !(8..=28).contains(&prefix) {
        return Err(t!("settings.error.subnet_prefix", prefix = prefix).into_owned());
    }
    let mask = u32::MAX << (32 - prefix);
    let network = Ipv4Addr::from(u32::from(address) & mask);
    if network != address {
        return Err(t!(
            "settings.error.subnet_host_bits",
            value = value,
            network = network,
            prefix = prefix
        )
        .into_owned());
    }
    if address.is_loopback() || address.is_multicast() || address.is_unspecified() {
        return Err(t!("settings.error.subnet_reserved", value = value).into_owned());
    }
    Ok((network, prefix))
}

/// UI language. Language names are shown in their own language so they
/// stay recognizable whatever the current locale is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum LanguagePreference {
    #[default]
    #[serde(rename = "system")]
    System,
    #[serde(rename = "en")]
    English,
    #[serde(rename = "zh-CN")]
    SimplifiedChinese,
}

impl LanguagePreference {
    pub const ALL: [Self; 3] = [Self::System, Self::English, Self::SimplifiedChinese];

    pub fn label(self) -> String {
        match self {
            Self::System => t!("settings.language.system").into(),
            Self::English => "English".into(),
            Self::SimplifiedChinese => "简体中文".into(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppSettings {
    pub theme: ThemePreference,
    pub language: LanguagePreference,
    pub docker_proxy: DockerProxySettings,
    /// Proxy for outbound traffic from containers (Docker Desktop's
    /// "Containers proxy").
    pub container_proxy: DockerProxySettings,
    pub wsl: WslSettings,
    pub resource_saver: ResourceSaverSettings,
    pub vm: VmSettings,
    pub network: NetworkSettings,
    /// "Don't ask again" for switching the Docker CLI to the Rusker context.
    pub skip_docker_context_prompt: bool,
}

impl AppSettings {
    pub fn validate(&self) -> Result<(), String> {
        if self.docker_proxy.mode == ProxyMode::SameAsHost {
            return Err(t!("settings.error.engine_proxy_same_as_host").into_owned());
        }
        self.docker_proxy
            .validate()
            .map_err(|error| t!("settings.error.engine_proxy", error = error).into_owned())?;
        self.container_proxy
            .validate()
            .map_err(|error| t!("settings.error.container_proxy", error = error).into_owned())?;
        self.resource_saver.validate()?;
        self.vm.validate()?;
        self.network.validate()
    }

    pub fn normalized(&self) -> Self {
        Self {
            theme: self.theme,
            language: self.language,
            docker_proxy: self.docker_proxy.normalized(),
            container_proxy: self.container_proxy.normalized(),
            wsl: self.wsl.normalized(),
            resource_saver: self.resource_saver.clone(),
            vm: self.vm.normalized(),
            network: self.network.normalized(),
            skip_docker_context_prompt: self.skip_docker_context_prompt,
        }
    }

    /// The container proxy addresses right now ("same as host" follows the
    /// Engine proxy).
    pub fn container_proxy_resolved(&self) -> ResolvedProxy {
        self.container_proxy.resolve(Some(&self.docker_proxy))
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
                return Err(SettingsError::io(
                    t!("settings.action.read_settings"),
                    path,
                    source,
                ));
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
        fs::create_dir_all(&self.root).map_err(|source| {
            SettingsError::io(t!("settings.action.create_config_dir"), &self.root, source)
        })?;
        let mut options = OpenOptions::new();
        options.create(true).truncate(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(path).map_err(|source| {
            SettingsError::io(t!("settings.action.write_settings"), path, source)
        })?;
        file.write_all(content.as_bytes()).map_err(|source| {
            SettingsError::io(t!("settings.action.write_settings"), path, source)
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|source| {
                SettingsError::io(t!("settings.action.set_permissions"), path, source)
            })?;
        }
        Ok(())
    }
}

/// The operating system's proxy: on Windows the WinINet settings (what
/// "Settings > Network > Proxy" edits), elsewhere the proxy environment
/// variables. Addresses that Docker could not use are dropped.
pub fn system_proxy() -> ResolvedProxy {
    #[cfg(windows)]
    let proxy = windows_system_proxy().unwrap_or_default();
    #[cfg(not(windows))]
    let proxy = {
        let var = |names: [&str; 2]| {
            names
                .iter()
                .find_map(|name| {
                    std::env::var(name)
                        .ok()
                        .filter(|value| !value.trim().is_empty())
                })
                .unwrap_or_default()
        };
        ResolvedProxy {
            http_proxy: var(["HTTP_PROXY", "http_proxy"]),
            https_proxy: var(["HTTPS_PROXY", "https_proxy"]),
            no_proxy: var(["NO_PROXY", "no_proxy"]),
        }
    };
    sanitize_proxy(proxy)
}

#[cfg(windows)]
fn windows_system_proxy() -> Option<ResolvedProxy> {
    let key = windows_registry::CURRENT_USER
        .open(r"Software\Microsoft\Windows\CurrentVersion\Internet Settings")
        .ok()?;
    if key.get_u32("ProxyEnable").ok()? == 0 {
        return None;
    }
    let server = key.get_string("ProxyServer").ok()?;
    let bypass = key.get_string("ProxyOverride").unwrap_or_default();
    Some(parse_windows_proxy(&server, &bypass))
}

/// Parses WinINet's `ProxyServer` (`host:port`, or per protocol
/// `http=host:port;https=host:port`) and `ProxyOverride`
/// (`localhost;127.*;<local>`).
#[cfg_attr(not(windows), allow(dead_code))]
fn parse_windows_proxy(server: &str, bypass: &str) -> ResolvedProxy {
    let with_scheme = |address: &str| {
        let address = address.trim();
        if address.is_empty() || address.contains("://") {
            address.to_string()
        } else {
            format!("http://{address}")
        }
    };
    let (http_proxy, https_proxy) = if server.contains('=') {
        let mut http = String::new();
        let mut https = String::new();
        for entry in server.split(';') {
            match entry.split_once('=') {
                Some((scheme, address)) if scheme.trim().eq_ignore_ascii_case("http") => {
                    http = with_scheme(address);
                }
                Some((scheme, address)) if scheme.trim().eq_ignore_ascii_case("https") => {
                    https = with_scheme(address);
                }
                _ => {}
            }
        }
        (http, https)
    } else {
        let address = with_scheme(server);
        (address.clone(), address)
    };
    let no_proxy = bypass
        .split(';')
        .filter_map(|entry| {
            let entry = entry.trim();
            match entry {
                "" | "<local>" => None,
                "127.*" => Some("127.0.0.0/8".into()),
                "10.*" => Some("10.0.0.0/8".into()),
                "192.168.*" => Some("192.168.0.0/16".into()),
                _ if entry.contains('*') && !entry.starts_with("*.") => None,
                _ => Some(entry.to_string()),
            }
        })
        .collect::<Vec<_>>()
        .join(",");
    ResolvedProxy {
        http_proxy,
        https_proxy,
        no_proxy,
    }
}

/// Drops addresses Docker would reject instead of failing the whole apply:
/// system settings are not editable here.
fn sanitize_proxy(proxy: ResolvedProxy) -> ResolvedProxy {
    let keep = |label: &str, value: String| {
        let value = value.trim().to_string();
        if validate_proxy_url(label, &value).is_ok() {
            value
        } else {
            String::new()
        }
    };
    let proxy = ResolvedProxy {
        http_proxy: keep("HTTP", proxy.http_proxy),
        https_proxy: keep("HTTPS", proxy.https_proxy),
        no_proxy: proxy
            .no_proxy
            .split(',')
            .map(str::trim)
            .filter(|entry| validate_no_proxy(entry).is_ok() && !entry.is_empty())
            .collect::<Vec<_>>()
            .join(","),
    };
    if proxy.is_empty() {
        ResolvedProxy::default()
    } else {
        proxy
    }
}

/// The Docker CLI's `config.json`: `$DOCKER_CONFIG/config.json`, otherwise
/// `~/.docker/config.json`.
pub fn docker_client_config_path() -> Option<PathBuf> {
    if let Some(directory) = std::env::var_os("DOCKER_CONFIG").filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(directory).join("config.json"));
    }
    directories::UserDirs::new().map(|dirs| dirs.home_dir().join(".docker").join("config.json"))
}

/// Writes the container proxy into `proxies.default` of the Docker CLI
/// config at `path`, leaving every other key untouched (an empty object is
/// removed when the proxy is disabled). Returns whether the file changed.
pub fn apply_client_proxy(path: &Path, proxy: &ResolvedProxy) -> Result<bool, SettingsError> {
    let current = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(source) => {
            return Err(SettingsError::io(
                t!("settings.action.read_client_config"),
                path,
                source,
            ))
        }
    };
    let Some(updated) = merge_client_proxies(&current, &proxy.client_proxies())
        .map_err(SettingsError::Validation)?
    else {
        return Ok(false);
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| {
            SettingsError::io(
                t!("settings.action.create_client_config_dir"),
                parent,
                source,
            )
        })?;
    }
    fs::write(path, updated).map_err(|source| {
        SettingsError::io(t!("settings.action.write_client_config"), path, source)
    })?;
    Ok(true)
}

/// Returns the new `config.json` text when `proxies.default` must change.
fn merge_client_proxies(
    current: &str,
    desired: &Map<String, Value>,
) -> Result<Option<String>, String> {
    let mut document = if current.trim().is_empty() {
        Map::new()
    } else {
        match serde_json::from_str::<Value>(current) {
            Ok(Value::Object(map)) => map,
            Ok(_) => return Err(t!("settings.error.client_config_not_object").into_owned()),
            Err(error) => {
                return Err(t!("settings.error.client_config_invalid", error = error).into_owned())
            }
        }
    };
    let existing = document
        .get("proxies")
        .and_then(|proxies| proxies.get("default"))
        .cloned();
    let wanted = (!desired.is_empty()).then(|| Value::Object(desired.clone()));
    if existing == wanted {
        return Ok(None);
    }
    let proxies = document
        .entry("proxies")
        .or_insert_with(|| Value::Object(Map::new()));
    let Value::Object(proxies) = proxies else {
        return Err(t!("settings.error.client_proxies_not_object").into_owned());
    };
    match wanted {
        Some(value) => {
            proxies.insert("default".into(), value);
        }
        None => {
            proxies.remove("default");
            if proxies.is_empty() {
                document.remove("proxies");
            }
        }
    }
    let formatted = serde_json::to_string_pretty(&Value::Object(document))
        .map_err(|error| error.to_string())?;
    Ok(Some(format!("{formatted}\n")))
}

#[derive(Debug, Error)]
pub enum SettingsError {
    #[error("{}", t!("settings.error.directory_unavailable"))]
    DirectoryUnavailable,
    #[error("{}", t!("settings.error.io", action = action, path = path.display(), source = source))]
    Io {
        action: String,
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{}", t!("settings.error.json", path = path.display(), source = source))]
    Json {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("{}", t!("settings.error.validation", detail = .0))]
    Validation(String),
}

impl SettingsError {
    fn io(action: impl Into<String>, path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            action: action.into(),
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
    let url = Url::parse(value).map_err(|error| {
        t!(
            "settings.error.proxy_url_invalid",
            label = label,
            error = error
        )
        .into_owned()
    })?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(t!("settings.error.proxy_url_scheme", label = label).into_owned());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(t!("settings.error.proxy_url_credentials", label = label).into_owned());
    }
    if url.query().is_some()
        || url.fragment().is_some()
        || !url.path().is_empty() && url.path() != "/"
    {
        return Err(t!("settings.error.proxy_url_path", label = label).into_owned());
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
            return Err(t!("settings.error.no_proxy_entry", entry = entry).into_owned());
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

    #[test]
    fn network_settings_map_to_daemon_keys() {
        // Defaults leave every network key to Docker.
        assert!(NetworkSettings::default().engine_config().is_empty());
        let legacy: AppSettings = serde_json::from_str(r#"{"theme":"dark"}"#).expect("旧设置");
        assert_eq!(legacy.network, NetworkSettings::default());

        let settings = NetworkSettings {
            subnet: "10.200.0.0/24".into(),
            host_networking: true,
            port_binding: PortBinding::LocalhostOnly,
            mode: NetworkingMode::DualStack,
        };
        let config = settings.engine_config();
        assert_eq!(config["bip"], "10.200.0.1/24");
        assert_eq!(config["ip"], "127.0.0.1");
        assert_eq!(config["ipv6"], true);
        assert_eq!(config["fixed-cidr-v6"], DEFAULT_BRIDGE_IPV6_SUBNET);
        assert_eq!(
            config["default-network-opts"]["bridge"]["com.docker.network.enable_ipv6"],
            "true"
        );
        assert!(config
            .keys()
            .all(|key| ENGINE_NETWORK_KEYS.contains(&key.as_str())));
        let saved = serde_json::to_value(&settings).expect("序列化");
        assert_eq!(saved["port_binding"], "localhost-only");
        assert_eq!(saved["mode"], "dual-stack");
    }

    #[test]
    fn docker_subnet_must_be_an_ipv4_network() {
        for (subnet, valid) in [
            ("", true),
            ("172.17.0.0/16", true),
            ("192.168.65.0/24", true),
            ("10.0.0.0/8", true),
            ("192.168.65.1/24", false),
            ("192.168.65.0", false),
            ("192.168.65.0/29", false),
            ("10.0.0.0/7", false),
            ("fd00::/64", false),
            ("127.0.0.0/8", false),
            ("not-a-subnet", false),
        ] {
            let settings = NetworkSettings {
                subnet: subnet.into(),
                ..NetworkSettings::default()
            };
            assert_eq!(settings.validate().is_ok(), valid, "子网 {subnet}");
        }
    }

    fn proxy() -> DockerProxySettings {
        DockerProxySettings {
            mode: ProxyMode::Manual,
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
        assert_eq!(value["proxies"]["no-proxy"], "localhost,127.0.0.1,::1");
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
            mode: ProxyMode::Manual,
            ..DockerProxySettings::default()
        };
        assert!(settings.validate().is_err());
    }

    #[test]
    fn container_env_sets_upper_and_lowercase_variables() {
        let env = proxy().resolve(None).container_env();
        assert!(env.contains(&"HTTP_PROXY=http://127.0.0.1:8080".to_string()));
        assert!(env.contains(&"http_proxy=http://127.0.0.1:8080".to_string()));
        assert!(env.contains(&"no_proxy=localhost,127.0.0.1,::1".to_string()));
        assert!(DockerProxySettings::default()
            .resolve(None)
            .container_env()
            .is_empty());
    }

    #[test]
    fn same_as_host_follows_the_engine_proxy() {
        let container = DockerProxySettings {
            mode: ProxyMode::SameAsHost,
            http_proxy: "http://ignored:1".into(),
            ..DockerProxySettings::default()
        };
        assert_eq!(container.resolve(Some(&proxy())), proxy().resolve(None));
        assert!(container
            .resolve(Some(&DockerProxySettings::default()))
            .is_empty());
        // Manual fields are kept but unused while another mode is selected.
        let mut none = proxy();
        none.mode = ProxyMode::None;
        assert!(none.resolve(None).is_empty());
        assert!(none.validate().is_ok());
    }

    #[test]
    fn legacy_enabled_flag_maps_to_proxy_modes() {
        let manual: DockerProxySettings =
            serde_json::from_str(r#"{"enabled":true,"http_proxy":"http://a:1"}"#).expect("旧格式");
        assert_eq!(manual.mode, ProxyMode::Manual);
        assert_eq!(manual.http_proxy, "http://a:1");
        let off: DockerProxySettings =
            serde_json::from_str(r#"{"enabled":false}"#).expect("旧格式");
        assert_eq!(off.mode, ProxyMode::None);
        let system: DockerProxySettings =
            serde_json::from_str(r#"{"mode":"system"}"#).expect("新格式");
        assert_eq!(system.mode, ProxyMode::System);
        let saved = serde_json::to_value(&system).expect("序列化");
        assert_eq!(saved["mode"], "system");
        assert!(saved.get("enabled").is_none());
        let app: AppSettings = serde_json::from_str(r#"{"theme":"dark"}"#).expect("旧设置");
        assert_eq!(app.container_proxy.mode, ProxyMode::None);
    }

    #[test]
    fn engine_proxy_cannot_be_same_as_host() {
        let mut settings = AppSettings::default();
        settings.docker_proxy.mode = ProxyMode::SameAsHost;
        assert!(settings.validate().is_err());
    }

    #[test]
    fn parses_windows_system_proxy() {
        let proxy = parse_windows_proxy(
            "127.0.0.1:7890",
            "localhost;127.*;10.*;<local>;*.corp.example;192.168.*",
        );
        assert_eq!(proxy.http_proxy, "http://127.0.0.1:7890");
        assert_eq!(proxy.https_proxy, "http://127.0.0.1:7890");
        assert_eq!(
            proxy.no_proxy,
            "localhost,127.0.0.0/8,10.0.0.0/8,*.corp.example,192.168.0.0/16"
        );

        let per_protocol =
            parse_windows_proxy("http=proxy:8080;https=secure:8443;socks=s:1080", "");
        assert_eq!(per_protocol.http_proxy, "http://proxy:8080");
        assert_eq!(per_protocol.https_proxy, "http://secure:8443");
        assert!(per_protocol.no_proxy.is_empty());

        // Only a SOCKS proxy: nothing Docker can use.
        let socks = sanitize_proxy(parse_windows_proxy("socks=s:1080", "localhost"));
        assert_eq!(socks, ResolvedProxy::default());
        let scheme = sanitize_proxy(parse_windows_proxy("socks5://s:1080", ""));
        assert!(scheme.is_empty());
    }

    #[test]
    fn merges_client_proxies_without_touching_other_keys() {
        let current = r#"{"auths":{"x":{}},"proxies":{"other":{"httpProxy":"http://a:1"}}}"#;
        let desired = proxy().resolve(None).client_proxies();
        let merged = merge_client_proxies(current, &desired)
            .expect("合并")
            .expect("应有变化");
        let value: Value = serde_json::from_str(&merged).expect("JSON");
        assert!(value["auths"]["x"].is_object());
        assert_eq!(value["proxies"]["other"]["httpProxy"], "http://a:1");
        assert_eq!(
            value["proxies"]["default"]["httpProxy"],
            "http://127.0.0.1:8080"
        );
        assert_eq!(
            value["proxies"]["default"]["noProxy"],
            "localhost,127.0.0.1,::1"
        );
        assert_eq!(merge_client_proxies(&merged, &desired).expect("合并"), None);

        // Disabling removes only `proxies.default`.
        let disabled = merge_client_proxies(&merged, &Map::new())
            .expect("合并")
            .expect("应有变化");
        let value: Value = serde_json::from_str(&disabled).expect("JSON");
        assert!(value["proxies"].get("default").is_none());
        assert!(value["proxies"]["other"].is_object());
        let only_default = r#"{"proxies":{"default":{"httpProxy":"http://a:1"}}}"#;
        let cleared = merge_client_proxies(only_default, &Map::new())
            .expect("合并")
            .expect("应有变化");
        assert_eq!(cleared.trim(), "{}");

        assert_eq!(merge_client_proxies("", &Map::new()).expect("空文件"), None);
        assert!(merge_client_proxies("{broken", &desired).is_err());
        assert!(merge_client_proxies("[]", &desired).is_err());
    }

    #[test]
    fn apply_client_proxy_creates_and_updates_config_file() {
        let directory = tempfile::tempdir().expect("临时目录");
        let path = directory.path().join(".docker").join("config.json");
        assert!(!apply_client_proxy(&path, &ResolvedProxy::default()).expect("无需写入"));
        assert!(!path.exists());
        assert!(apply_client_proxy(&path, &proxy().resolve(None)).expect("写入"));
        assert!(!apply_client_proxy(&path, &proxy().resolve(None)).expect("无变化"));
        let value: Value =
            serde_json::from_str(&fs::read_to_string(&path).expect("读取")).expect("JSON");
        assert_eq!(
            value["proxies"]["default"]["httpsProxy"],
            "https://proxy.example.com:8443"
        );
    }

    #[test]
    fn settings_round_trip_uses_user_supplied_directory() {
        let directory = tempfile::tempdir().expect("临时目录");
        let store = SettingsStore::at(directory.path());
        assert_eq!(store.load().expect("默认设置"), AppSettings::default());

        let settings = AppSettings {
            theme: ThemePreference::Dark,
            language: LanguagePreference::default(),
            docker_proxy: proxy(),
            container_proxy: DockerProxySettings::default(),
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
            network: NetworkSettings {
                subnet: " 10.200.0.0/24 ".into(),
                host_networking: true,
                port_binding: PortBinding::LocalhostOnly,
                mode: NetworkingMode::DualStack,
            },
            skip_docker_context_prompt: true,
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
        assert_eq!(loaded.network.subnet, "10.200.0.0/24");
        assert_eq!(loaded.network.port_binding, PortBinding::LocalhostOnly);
        assert_eq!(loaded.network.mode, NetworkingMode::DualStack);
        assert!(loaded.network.host_networking);
        assert!(loaded.skip_docker_context_prompt);

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
