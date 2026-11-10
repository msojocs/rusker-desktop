use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::{Output, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        Arc, Mutex, MutexGuard,
    },
    time::Duration,
};

use directories::ProjectDirs;
use serde_json::{Map, Value};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::{Child, Command},
    time::timeout,
};

use super::{
    data_disk,
    engine_proxy::{self, EngineProxyState},
    lan_forward::LanForwardStatus,
    RuntimeHostError,
};
use crate::settings::{
    DockerProxySettings, NetworkSettings, ResourceSaverSettings, ENGINE_NETWORK_KEYS,
};

pub const MANAGED_DISTRO_NAME: &str = "rusker";
/// Windows-side endpoint, served by the in-app proxy (like Docker Desktop's
/// `\\.\pipe\docker_engine`).
pub const MANAGED_PIPE: &str = r"\\.\pipe\rusker_engine";
/// `DOCKER_HOST` value for Windows tools such as `docker.exe`.
pub const MANAGED_DOCKER_HOST: &str = "npipe:////./pipe/rusker_engine";
/// `DOCKER_HOST` value inside integrated WSL2 distributions. `/mnt/wsl` is a
/// tmpfs shared by every WSL2 distribution in the same utility VM.
pub const WSL_DOCKER_HOST: &str = "unix:///mnt/wsl/rusker/docker.sock";
const MANAGED_MARKER: &str = "/etc/rusker-managed";
const MANAGED_RECEIPT: &str = "rusker-wsl2-v1\n";
/// Version of the Rusker system image (`images/wsl-rootfs/VERSION`, via
/// build.rs) this build expects, as recorded in the image's
/// `/etc/rusker-image`.
const ROOTFS_IMAGE: &str = concat!("rusker-rootfs-v", env!("RUSKER_ROOTFS_VERSION"));
/// Where the Windows installer puts the system image, relative to the exe,
/// like Docker Desktop's `resources\docker-desktop.iso`.
const BUNDLED_ROOTFS_DIR: &str = "resources/wsl";
pub(super) const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const INSTALL_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(3 * 60);
const READY_TIMEOUT: Duration = Duration::from_secs(90);
const DOCKERD_STOP_TIMEOUT: Duration = Duration::from_secs(45);

/// Checks a freshly imported system image. Everything (Docker Engine,
/// Compose/Buildx, NVIDIA Container Toolkit, `wsl.conf`, the ownership
/// marker) is baked into the image, so installing needs no network inside
/// the distribution. `$1` is the expected image version.
const VERIFY_IMAGE_SCRIPT: &str = r#"set -eu
if ! grep -qx "RUSKER_IMAGE=$1" /etc/rusker-image 2>/dev/null; then
    echo "专用发行版不是 Rusker 系统镜像 $1" >&2
    exit 1
fi
for file in /usr/bin/dockerd /usr/bin/docker /usr/bin/containerd /usr/bin/runc \
    /usr/bin/nvidia-ctk /usr/bin/nvidia-container-runtime; do
    if [ ! -x "$file" ]; then
        echo "系统镜像缺少 $file" >&2
        exit 1
    fi
done
grep -q '"nvidia"' /etc/docker/daemon.json
"#;

/// Prints the system image version of the managed distribution; empty for
/// an install from before the Rusker image (plain Ubuntu plus apt).
const IMAGE_VERSION_SCRIPT: &str =
    "sed -n 's/^RUSKER_IMAGE=//p' /etc/rusker-image 2>/dev/null || true";

/// Before the system disk is replaced: succeeds only when Docker's state is
/// on the data disk (mounted and migrated), so nothing on the old system
/// disk is lost.
const DATA_ON_DATA_DISK_SCRIPT: &str = r#"set -eu
for name in docker containerd; do
    mountpoint -q "/var/lib/$name"
    [ -e "/mnt/rusker-data/.migrated-$name" ]
done
"#;

/// Long-lived supervisor, run through a `wsl.exe` session that the backend
/// keeps open (WSL2 terminates a distribution ~10 s after its last session
/// closes, regardless of background processes). If a dockerd from an earlier
/// session is still alive, the supervisor attaches to it instead of starting a
/// second daemon. No TCP listener is opened.
const SUPERVISOR_SCRIPT: &str = r#"set -u
LOG=/var/log/rusker-dockerd.log
SHARED=/mnt/wsl/rusker
mkdir -p /var/log /var/run "$SHARED"
if [ -f "$LOG" ] && [ "$(stat -c %s "$LOG")" -gt 10485760 ]; then
    mv -f "$LOG" "$LOG.1"
fi
exec >>"$LOG" 2>&1
echo "[rusker] supervisor started $(date -Is)"
# Re-mount the Docker data disk in case the distribution restarted since
# the backend prepared it; a no-op when it is already mounted.
(
__MOUNT_DATA_DISK__
) || { echo "[rusker] Docker 数据磁盘未就绪"; exit 1; }
pid=$(cat /var/run/docker.pid 2>/dev/null || true)
if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null && grep -qx dockerd "/proc/$pid/comm" 2>/dev/null; then
    echo "[rusker] attaching to running dockerd $pid"
    while kill -0 "$pid" 2>/dev/null; do sleep 2; done
    echo "[rusker] dockerd $pid exited"
    exit 0
fi
(
    i=0
    while [ "$i" -lt 240 ]; do
        if [ -S "$SHARED/docker.sock" ]; then
            chmod 0666 "$SHARED/docker.sock"
            exit 0
        fi
        sleep 0.5
        i=$((i + 1))
    done
) &
exec /usr/bin/dockerd --host=unix:///var/run/docker.sock --host="unix://$SHARED/docker.sock"
"#;

/// Atomically replaces the managed `daemon.json` with stdin, after checking
/// it with `dockerd --validate` so a bad config never replaces a working one.
/// Older dockerd builds without `--validate` skip the check.
const VALIDATE_AND_WRITE_DAEMON_JSON_SCRIPT: &str = r#"set -u
mkdir -p /etc/docker
NEW=/etc/docker/daemon.json.rusker-new
cat > "$NEW" || exit 1
if ! out=$(/usr/bin/dockerd --validate --config-file "$NEW" 2>&1); then
    case "$out" in
        *"unknown flag"*) ;;
        *) rm -f "$NEW"; printf '%s\n' "$out" >&2; exit 1 ;;
    esac
fi
chmod 0644 "$NEW"
mv -f "$NEW" /etc/docker/daemon.json
"#;

/// Only signal the exact processes recorded at pause time, never a reused PID.
/// Kept outside the image so existing installations gain Resource Saver too.
const ENGINE_SIGNALS_SCRIPT: &str = r#"set -eu
PAUSED=/var/run/rusker-engine.paused
process_start() { awk '{print $22}' "/proc/$1/stat" 2>/dev/null; }
resume_engine() {
    [ -f "$PAUSED" ] || return 0
    failed=0
    while read -r pid started; do
        case "$pid" in ''|*[!0-9]*) continue ;; esac
        current=$(process_start "$pid" || true)
        if [ -n "$current" ] && [ "$current" = "$started" ]; then
            kill -CONT "$pid" || failed=1
        fi
    done < "$PAUSED"
    [ "$failed" -eq 0 ] || return 1
    rm -f "$PAUSED"
}
"#;

/// Pause only an idle dockerd and its own containerd, not the WSL VM or
/// unrelated services. The session, sockets and data mounts stay alive.
const PAUSE_SCRIPT: &str = r#"resume_engine
running=$(/usr/bin/docker --host=unix:///var/run/docker.sock ps -q)
[ -z "$running" ] || exit 75
pid=$(cat /var/run/docker.pid)
case "$pid" in ''|*[!0-9]*) exit 1 ;; esac
grep -qx dockerd "/proc/$pid/comm"
process_start "$pid" > /dev/null
: > "$PAUSED"
trap 'resume_engine' EXIT HUP INT TERM
printf '%s %s\n' "$pid" "$(process_start "$pid")" >> "$PAUSED"
kill -STOP "$pid"
# dockerd can no longer create children; freeze only its managed containerd.
for child in $(cat /proc/"$pid"/task/*/children); do
    if grep -qx containerd "/proc/$child/comm" 2>/dev/null; then
        printf '%s %s\n' "$child" "$(process_start "$child")" >> "$PAUSED"
        kill -STOP "$child"
    fi
done
trap - EXIT HUP INT TERM
"#;

const STOP_SCRIPT: &str = r#"set -u
pid=$(cat /var/run/docker.pid 2>/dev/null || true)
if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
    kill -TERM "$pid"
    i=0
    while kill -0 "$pid" 2>/dev/null && [ "$i" -lt 40 ]; do
        sleep 1
        i=$((i + 1))
    done
fi
"#;

/// Runs inside the managed distribution. `$1` is `fingerprint` or `archive`.
const MANAGED_CLI_SCRIPT: &str = r#"set -eu
mode=$1
cd /
set -- usr/bin/docker
for plugin in docker-compose docker-buildx; do
    if [ -x "usr/libexec/docker/cli-plugins/$plugin" ]; then
        set -- "$@" "usr/libexec/docker/cli-plugins/$plugin"
    fi
done
case "$mode" in
    fingerprint) sha256sum "$@" | sha256sum | cut -c1-64 ;;
    archive) exec tar -cf - "$@" ;;
    *) exit 64 ;;
esac
"#;

/// Prints the installed fingerprint only when the integration is intact and
/// already targets the shared socket.
const INTEGRATION_STATE_SCRIPT: &str = r#"if [ -f /usr/local/lib/rusker/integrated ] \
    && [ -f /usr/local/bin/docker ] \
    && grep -q 'RUSKER_MANAGED_DOCKER_WRAPPER' /usr/local/bin/docker \
    && grep -q 'unix:///mnt/wsl/rusker/docker.sock' /usr/local/bin/docker; then
    cat /usr/local/lib/rusker/fingerprint 2>/dev/null || true
fi
"#;

/// Runs inside the target distribution with a tar archive of the managed CLI
/// on stdin. `$1` is the managed CLI fingerprint.
const INSTALL_CLI_SCRIPT: &str = r#"set -eu
fingerprint=$1
if [ -e /usr/local/bin/docker ] || [ -L /usr/local/bin/docker ]; then
    if [ ! -f /usr/local/lib/rusker/integrated ] || ! grep -q 'RUSKER_MANAGED_DOCKER_WRAPPER' /usr/local/bin/docker; then
        echo '/usr/local/bin/docker 已存在或已被修改，拒绝覆盖' >&2
        exit 73
    fi
fi
staging=$(mktemp -d)
trap 'rm -rf "$staging"' EXIT
tar -xf - -C "$staging"
mkdir -p /usr/local/lib/rusker/cli-plugins /usr/local/bin /usr/local/lib/docker/cli-plugins
install -m 0755 "$staging/usr/bin/docker" /usr/local/lib/rusker/docker
for plugin in docker-compose docker-buildx; do
    source="$staging/usr/libexec/docker/cli-plugins/$plugin"
    target="/usr/local/lib/rusker/cli-plugins/$plugin"
    link="/usr/local/lib/docker/cli-plugins/$plugin"
    [ -f "$source" ] || continue
    install -m 0755 "$source" "$target"
    if [ -L "$link" ] && [ "$(readlink "$link")" = "$target" ]; then
        continue
    fi
    if [ -e "$link" ] || [ -L "$link" ]; then
        echo "$link 已存在，保留发行版自带的 $plugin" >&2
        continue
    fi
    ln -s "$target" "$link"
done
cat > /usr/local/bin/docker.rusker-new <<'RUSKER_WRAPPER'
#!/bin/sh
# RUSKER_MANAGED_DOCKER_WRAPPER
if [ -z "${DOCKER_HOST:-}" ] && [ -z "${DOCKER_CONTEXT:-}" ]; then
    DOCKER_HOST='unix:///mnt/wsl/rusker/docker.sock'
    export DOCKER_HOST
fi
exec /usr/local/lib/rusker/docker "$@"
RUSKER_WRAPPER
chmod 0755 /usr/local/bin/docker.rusker-new
mv -f /usr/local/bin/docker.rusker-new /usr/local/bin/docker
printf '%s\n' "$fingerprint" > /usr/local/lib/rusker/fingerprint
printf 'rusker-wsl2-integration-v2\n' > /usr/local/lib/rusker/integrated
"#;

const REMOVE_CLI_SCRIPT: &str = r#"set -eu
if [ ! -f /usr/local/lib/rusker/integrated ]; then
    exit 0
fi
if [ -e /usr/local/bin/docker ] || [ -L /usr/local/bin/docker ]; then
    if ! grep -q 'RUSKER_MANAGED_DOCKER_WRAPPER' /usr/local/bin/docker; then
        echo '/usr/local/bin/docker 已被其他程序修改，保留该文件' >&2
        exit 74
    fi
    rm -f /usr/local/bin/docker
fi
for plugin in docker-compose docker-buildx; do
    link="/usr/local/lib/docker/cli-plugins/$plugin"
    if [ -L "$link" ] && [ "$(readlink "$link")" = "/usr/local/lib/rusker/cli-plugins/$plugin" ]; then
        rm -f "$link"
    fi
    rm -f "/usr/local/lib/rusker/cli-plugins/$plugin"
done
rmdir /usr/local/lib/docker/cli-plugins /usr/local/lib/docker 2>/dev/null || true
rm -f /usr/local/lib/rusker/docker /usr/local/lib/rusker/docker.new /usr/local/lib/rusker/fingerprint /usr/local/lib/rusker/integrated
rmdir /usr/local/lib/rusker/cli-plugins /usr/local/lib/rusker 2>/dev/null || true
"#;

const GPU_PROBE_SCRIPT: &str = r#"set -eu
if [ ! -e /dev/dxg ]; then
    echo '未检测到 /dev/dxg；请确认 Windows NVIDIA 驱动支持 WSL2' >&2
    exit 1
fi
if [ ! -x /usr/bin/nvidia-ctk ]; then
    echo 'NVIDIA Container Toolkit 未安装，请修复运行环境' >&2
    exit 1
fi
/usr/lib/wsl/lib/nvidia-smi --query-gpu=name --format=csv,noheader
"#;

#[derive(Clone, Debug, Default)]
pub struct NvidiaGpuStatus {
    pub available: bool,
    pub name: Option<String>,
    pub detail: String,
}

#[derive(Clone, Debug)]
pub struct WslDistribution {
    pub name: String,
    pub state: String,
    pub is_default: bool,
    pub integration_enabled: bool,
}

/// Automatic restart of an engine that went away unexpectedly, like Docker
/// Desktop restarting its WSL distribution after `wsl --shutdown`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum EngineRecovery {
    #[default]
    Idle,
    /// The session ended unexpectedly and is being restarted.
    Restarting { attempt: u32 },
    /// Gave up after repeated failures; a manual start is needed.
    Failed(String),
}

#[derive(Default)]
pub(crate) struct RecoveryStatus {
    /// The state, and how many recoveries have succeeded so far (the app
    /// reconnects when it changes).
    state: Mutex<(EngineRecovery, u64)>,
}

impl RecoveryStatus {
    pub(crate) fn get(&self) -> (EngineRecovery, u64) {
        self.state.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    fn set(&self, recovery: EngineRecovery) {
        self.state.lock().unwrap_or_else(|p| p.into_inner()).0 = recovery;
    }

    fn recovered(&self) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state.0 = EngineRecovery::Idle;
        state.1 += 1;
    }

    /// A successful (manual) start ends a failed recovery.
    fn clear_failure(&self) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if matches!(state.0, EngineRecovery::Failed(_)) {
            state.0 = EngineRecovery::Idle;
        }
    }
}

#[derive(Clone, Debug)]
pub struct RuntimeHostSnapshot {
    pub supported: bool,
    pub installed: bool,
    pub running: bool,
    pub engine_ready: bool,
    pub gpu: NvidiaGpuStatus,
    pub distributions: Vec<WslDistribution>,
    pub warnings: Vec<String>,
    /// The engine is paused in its live WSL session and resumes on the next
    /// Windows client connection.
    pub resource_saver_active: bool,
    /// Directory holding the managed distribution's `ext4.vhdx`.
    pub disk_location: Option<PathBuf>,
}

// `supported` is only `true` on Windows; elsewhere every field is the type
// default and clippy would suggest deriving.
#[cfg_attr(not(windows), allow(clippy::derivable_impls))]
impl Default for RuntimeHostSnapshot {
    fn default() -> Self {
        Self {
            supported: cfg!(windows),
            installed: false,
            running: false,
            engine_ready: false,
            gpu: NvidiaGpuStatus::default(),
            distributions: Vec::new(),
            warnings: Vec::new(),
            resource_saver_active: false,
            disk_location: None,
        }
    }
}

/// Resource Saver configuration and live state, shared between the idle
/// monitor, the named pipe proxy (which wakes the engine) and the UI.
pub(crate) struct ResourceSaver {
    enabled: AtomicBool,
    threshold_secs: AtomicU64,
    active: AtomicBool,
    integrations_enabled: AtomicBool,
    connections: AtomicUsize,
}

impl Default for ResourceSaver {
    fn default() -> Self {
        let defaults = ResourceSaverSettings::default();
        Self {
            // Off until the app pushes the saved settings.
            enabled: AtomicBool::new(false),
            threshold_secs: AtomicU64::new(defaults.threshold_secs),
            active: AtomicBool::new(false),
            integrations_enabled: AtomicBool::new(false),
            connections: AtomicUsize::new(0),
        }
    }
}

impl ResourceSaver {
    pub(crate) fn configure(&self, settings: &ResourceSaverSettings) {
        self.threshold_secs
            .store(settings.threshold_secs, Ordering::Relaxed);
        self.enabled.store(settings.enabled, Ordering::Relaxed);
    }

    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    pub(crate) fn set_integrations(&self, integrations: &BTreeMap<String, bool>) {
        self.integrations_enabled.store(
            integrations.values().any(|enabled| *enabled),
            Ordering::SeqCst,
        );
    }

    pub(crate) fn can_pause(&self) -> bool {
        self.enabled() && !self.integrations_enabled.load(Ordering::SeqCst)
    }

    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) fn threshold(&self) -> Duration {
        Duration::from_secs(self.threshold_secs.load(Ordering::Relaxed))
    }

    pub(crate) fn is_active(&self) -> bool {
        self.active.load(Ordering::SeqCst)
    }

    fn set_active(&self, active: bool) {
        self.active.store(active, Ordering::SeqCst);
    }

    /// Open Windows pipe connections; any of them keeps the engine awake.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) fn connections(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }

    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) fn track_connection(self: &Arc<Self>) -> ConnectionGuard {
        self.connections.fetch_add(1, Ordering::SeqCst);
        ConnectionGuard(self.clone())
    }
}

pub(crate) struct ConnectionGuard(Arc<ResourceSaver>);

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.0.connections.fetch_sub(1, Ordering::SeqCst);
    }
}

/// The backend-held `wsl.exe` session that runs dockerd and keeps the managed
/// distribution alive. Dropping the child kills `wsl.exe` (`kill_on_drop`).
#[derive(Default)]
pub(crate) struct EngineSession {
    child: Mutex<Option<Child>>,
}

impl EngineSession {
    fn guard(&self) -> MutexGuard<'_, Option<Child>> {
        self.child
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub(crate) fn is_running(&self) -> bool {
        let mut guard = self.guard();
        let alive = match guard.as_mut() {
            Some(child) => matches!(child.try_wait(), Ok(None)),
            None => false,
        };
        if !alive {
            *guard = None;
        }
        alive
    }

    fn replace(&self, child: Child) {
        *self.guard() = Some(child);
    }

    fn take(&self) -> Option<Child> {
        self.guard().take()
    }
}

/// Last error from the named pipe proxy, surfaced as a snapshot warning.
#[derive(Default)]
pub(crate) struct ProxyStatus {
    error: Mutex<Option<String>>,
}

impl ProxyStatus {
    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) fn set_error(&self, message: String) {
        *self.error.lock().unwrap_or_else(|p| p.into_inner()) = Some(message);
    }

    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) fn clear(&self) {
        *self.error.lock().unwrap_or_else(|p| p.into_inner()) = None;
    }

    fn error(&self) -> Option<String> {
        self.error.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }
}

#[derive(Clone)]
pub struct WslHost {
    root: PathBuf,
    session: Arc<EngineSession>,
    proxy: Arc<ProxyStatus>,
    /// Proxy settings served by the Engine proxy relay (see
    /// `engine_proxy`); dockerd itself always points at the relay.
    engine_proxy: Arc<EngineProxyState>,
    /// Desired network keys ([`ENGINE_NETWORK_KEYS`]) of `daemon.json`.
    engine_network: Arc<Mutex<Map<String, Value>>>,
    saver: Arc<ResourceSaver>,
    lan_forward: Arc<LanForwardStatus>,
    /// Set once the engine started successfully, cleared by deliberate
    /// stops (manual, quit); Resource Saver retains the session and intent.
    engine_wanted: Arc<AtomicBool>,
    recovery: Arc<RecoveryStatus>,
    /// Where a new Docker data disk is created (None = default).
    data_location: Arc<Mutex<Option<PathBuf>>>,
}

/// What saving the Engine proxy settings did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProxyApplyOutcome {
    /// Engine not running; the config is written before its next start.
    Deferred,
    /// `daemon.json` already matched.
    Unchanged,
    /// `daemon.json` updated and dockerd restarted.
    Restarted,
}

impl WslHost {
    pub fn discover() -> Result<Self, RuntimeHostError> {
        let root = if let Some(path) = std::env::var_os("RUSKER_DATA_DIR") {
            PathBuf::from(path).join("runtime-host")
        } else {
            ProjectDirs::from("dev", "Rusker", "Rusker Desktop")
                .ok_or_else(|| {
                    RuntimeHostError::State(
                        t!("runtime_host.error.app_data_dir_unavailable").into_owned(),
                    )
                })?
                .data_local_dir()
                .join("runtime-host")
        };
        Ok(Self {
            root,
            session: Arc::default(),
            proxy: Arc::default(),
            engine_proxy: Arc::default(),
            engine_network: Arc::default(),
            saver: Arc::default(),
            lan_forward: Arc::default(),
            engine_wanted: Arc::default(),
            recovery: Arc::default(),
            data_location: Arc::default(),
        })
    }

    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) fn resource_saver(&self) -> Arc<ResourceSaver> {
        self.saver.clone()
    }

    pub fn session_running(&self) -> bool {
        self.session.is_running()
    }

    /// The engine should be running but its session is gone: WSL was shut
    /// down from outside (`wsl --shutdown`), the distribution was
    /// terminated, or dockerd crashed.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) fn engine_lost(&self) -> bool {
        self.engine_wanted.load(Ordering::Relaxed)
            && !self.saver.is_active()
            && !self.session.is_running()
    }

    pub(crate) fn recovery_status(&self) -> Arc<RecoveryStatus> {
        self.recovery.clone()
    }

    /// Restarts a lost engine. The caller holds the operation lock.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) async fn recover(&self, attempt: u32) -> Result<(), RuntimeHostError> {
        self.recovery.set(EngineRecovery::Restarting { attempt });
        self.start().await?;
        self.recovery.recovered();
        Ok(())
    }

    /// Stops recovering after repeated failures; a manual start is needed.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) fn give_up_recovery(&self, message: String) {
        self.engine_wanted.store(false, Ordering::Relaxed);
        self.recovery.set(EngineRecovery::Failed(message));
    }

    /// Default directory for the Docker data disk (`docker-data.vhdx`).
    pub fn default_disk_location(&self) -> PathBuf {
        self.root.join("disk")
    }

    /// Directory of the distribution's own system disk (`ext4.vhdx`).
    fn distro_location(&self) -> PathBuf {
        self.root.join("distro")
    }

    /// Sets where a new Docker data disk is created (None = default).
    pub fn set_data_location(&self, location: Option<PathBuf>) {
        *lock(&self.data_location) = location;
    }

    /// Directory currently holding the Docker data disk, if one exists.
    fn data_disk_location(&self) -> Option<PathBuf> {
        data_disk::load(&self.root)
            .ok()
            .flatten()
            .map(|disk| disk.directory())
    }

    /// Moves the Docker data disk like Docker Desktop: stop the engine,
    /// detach only this disk from the WSL2 VM, move the file, reattach.
    /// Other WSL distributions keep running.
    pub async fn move_disk(&self, target: &Path) -> Result<Vec<String>, RuntimeHostError> {
        ensure_windows()?;
        self.ensure_owned().await?;
        let Some(disk) = data_disk::load(&self.root)? else {
            // Not created yet: the next engine start creates it there.
            self.set_data_location(Some(target.to_path_buf()));
            return Ok(Vec::new());
        };
        if same_path(&disk.directory(), target) {
            return Ok(Vec::new());
        }
        if target.join(data_disk::DATA_DISK_FILE).exists() {
            return Err(RuntimeHostError::Conflict(
                t!(
                    "runtime_host.error.target_dir_has_existing",
                    path = data_disk::DATA_DISK_FILE
                )
                .into_owned(),
            ));
        }
        let was_running = self.session.is_running();
        self.stop_engine().await;
        let mut terminate = wsl_command();
        terminate.args(["--terminate", MANAGED_DISTRO_NAME]);
        execute(
            terminate,
            t!("runtime_host.action.stop_wsl_environment"),
            COMMAND_TIMEOUT,
        )
        .await?;
        self.detach_data_disk().await;
        let directory = target.to_path_buf();
        let moving = disk.clone();
        let moved = tokio::task::spawn_blocking(move || data_disk::move_file(&moving, &directory))
            .await
            .map_err(|error| RuntimeHostError::Task(error.to_string()))
            .and_then(|result| result)
            .and_then(|moved| {
                data_disk::save(&self.root, &moved)?;
                Ok(moved)
            });
        // Restore the engine either way: on failure the disk is still at
        // its old location and gets reattached from there.
        let restarted = if was_running {
            self.start_engine().await
        } else {
            Ok(())
        };
        moved?;
        self.set_data_location(Some(target.to_path_buf()));
        // The move itself succeeded, so the new location must be saved even
        // when the engine fails to come back.
        Ok(restarted
            .err()
            .map(|error| {
                t!(
                    "runtime_host.error.disk_moved_engine_not_restored",
                    error = error
                )
                .into_owned()
            })
            .into_iter()
            .collect())
    }

    /// Attaches the Docker data disk to the WSL2 VM (creating it on first
    /// use) and mounts it over `/var/lib/docker` and `/var/lib/containerd`.
    async fn attach_data_disk(&self) -> Result<String, RuntimeHostError> {
        let disk = match data_disk::load(&self.root)? {
            Some(disk) => disk,
            None => {
                let directory = lock(&self.data_location)
                    .clone()
                    .unwrap_or_else(|| self.default_disk_location());
                let disk = data_disk::create(&directory)?;
                if let Err(error) = data_disk::save(&self.root, &disk) {
                    let _ = fs::remove_file(&disk.path);
                    return Err(error);
                }
                disk
            }
        };
        if !disk.path.exists() {
            return Err(RuntimeHostError::Conflict(
                t!(
                    "runtime_host.error.disk_missing",
                    path = disk.path.display()
                )
                .into_owned(),
            ));
        }
        let wwid = disk.wwid()?;
        // Fails harmlessly when the disk is still attached from an earlier
        // start; the mount script below is the real check.
        let mut attach = wsl_command();
        attach
            .args(["--mount", "--vhd"])
            .arg(&disk.path)
            .arg("--bare");
        let attached = execute(
            attach,
            t!("runtime_host.action.mount_data_disk"),
            COMMAND_TIMEOUT,
        )
        .await;
        let mut mount = managed_command("root");
        mount
            .arg("/bin/sh")
            .arg("-c")
            .arg(data_disk::MOUNT_SCRIPT)
            .arg("rusker-data")
            .arg(&wwid);
        match execute(
            mount,
            t!("runtime_host.action.prepare_data_disk"),
            INSTALL_TIMEOUT,
        )
        .await
        {
            Ok(_) => Ok(wwid),
            Err(error) => Err(attached.err().unwrap_or(error)),
        }
    }

    /// Detaches the Docker data disk from the WSL2 VM so the file is free.
    /// Only call after the managed distribution was terminated.
    async fn detach_data_disk(&self) {
        let Ok(Some(disk)) = data_disk::load(&self.root) else {
            return;
        };
        let mut unmount = wsl_command();
        unmount.arg("--unmount").arg(&disk.path);
        let _ = execute(
            unmount,
            t!("runtime_host.action.unmount_data_disk"),
            COMMAND_TIMEOUT,
        )
        .await;
    }

    /// Number of running containers, asked inside the distribution so the
    /// check itself never counts as pipe activity.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) async fn running_containers(&self) -> Result<usize, RuntimeHostError> {
        let mut command = managed_command("root");
        command.args(["/usr/bin/docker", "ps", "--quiet"]);
        let output = execute(
            command,
            t!("runtime_host.action.check_running_containers"),
            COMMAND_TIMEOUT,
        )
        .await?;
        Ok(decode(&output.stdout)
            .lines()
            .filter(|line| !line.trim().is_empty())
            .count())
    }

    /// Pauses the idle engine without tearing down the WSL environment.
    /// The caller holds the operation lock, also used by new pipe clients.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) async fn enter_resource_saver(&self) -> Result<(), RuntimeHostError> {
        if !self.saver.can_pause()
            || self.saver.is_active()
            || self.saver.connections() > 0
            || !self.session.is_running()
        {
            return Ok(());
        }
        let mut pause = managed_command("root");
        pause.args([
            "/bin/sh",
            "-c",
            &format!("{ENGINE_SIGNALS_SCRIPT}{PAUSE_SCRIPT}"),
        ]);
        if let Err(error) = execute(
            pause,
            t!("runtime_host.action.pause_idle_engine"),
            COMMAND_TIMEOUT,
        )
        .await
        {
            // A timeout can kill wsl.exe before the shell's rollback trap ran.
            // Preserve the gate if rollback fails so clients retry resume,
            // rather than sending requests to a partially frozen daemon.
            if self.continue_engine().await.is_err() {
                self.saver.set_active(true);
            }
            return Err(error);
        }
        // Keep the flag until resume AND readiness succeed; a live session
        // is intentionally not sufficient to mean that requests can proceed.
        self.saver.set_active(true);
        Ok(())
    }

    /// Fast path: no ownership probe, disk attachment, config read or daemon
    /// restart. Concurrent clients wait on the caller's operation lock.
    pub(crate) async fn resume_resource_saver(&self) -> Result<(), RuntimeHostError> {
        if !self.saver.is_active() {
            return Ok(());
        }
        if !self.session.is_running() {
            // WSL was shut down externally while paused: cold start is only
            // a fallback, never the normal Resource Saver exit path.
            self.ensure_owned().await?;
            return self.start_engine().await;
        }
        self.continue_engine().await?;
        self.wait_until_ready().await?;
        self.saver.set_active(false);
        self.proxy.clear();
        Ok(())
    }

    async fn continue_engine(&self) -> Result<(), RuntimeHostError> {
        let mut resume = managed_command("root");
        resume.args([
            "/bin/sh",
            "-c",
            &format!("{ENGINE_SIGNALS_SCRIPT}resume_engine"),
        ]);
        execute(
            resume,
            t!("runtime_host.action.resume_engine"),
            COMMAND_TIMEOUT,
        )
        .await?;
        Ok(())
    }

    /// Like Docker Desktop, dockerd never sees the configured addresses: it
    /// always uses the in-VM relay, and the Windows side resolves these
    /// settings per request (so `127.0.0.1` means the Windows host even in
    /// NAT mode). Takes effect immediately, without a dockerd restart.
    pub fn set_engine_proxy(&self, settings: &DockerProxySettings) {
        self.engine_proxy.configure(settings);
    }

    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) fn engine_proxy_state(&self) -> Arc<EngineProxyState> {
        self.engine_proxy.clone()
    }

    fn desired_proxies(&self) -> Map<String, Value> {
        engine_proxy::daemon_proxies()
    }

    pub fn set_engine_network(&self, settings: &NetworkSettings) {
        *self
            .engine_network
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = settings.engine_config();
    }

    /// Values for every [`MANAGED_DAEMON_KEYS`] entry the settings set; the
    /// managed keys missing here are removed from `daemon.json`.
    fn desired_managed(&self) -> Map<String, Value> {
        let mut managed = self
            .engine_network
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        let proxies = self.desired_proxies();
        if !proxies.is_empty() {
            managed.insert("proxies".into(), Value::Object(proxies));
        }
        managed
    }

    /// Applies saved Engine settings (proxy and network) directly, like
    /// Docker Desktop's "Apply & restart": merge into `daemon.json` and
    /// restart a running dockerd if anything changed. With `daemon_config`
    /// (the user-edited `daemon.json` without the managed keys), the whole
    /// file is replaced too.
    pub async fn apply_engine_config(
        &self,
        proxy: &DockerProxySettings,
        network: &NetworkSettings,
        daemon_config: Option<&str>,
    ) -> Result<ProxyApplyOutcome, RuntimeHostError> {
        proxy.validate().map_err(RuntimeHostError::State)?;
        network.validate().map_err(RuntimeHostError::State)?;
        let user_config = daemon_config
            .map(parse_editable_daemon_config)
            .transpose()
            .map_err(RuntimeHostError::State)?;
        self.set_engine_proxy(proxy);
        self.set_engine_network(network);
        if !self.session.is_running() {
            if user_config.is_some() {
                return Err(RuntimeHostError::State(
                    t!("runtime_host.error.engine_not_running_cannot_modify").into_owned(),
                ));
            }
            return Ok(ProxyApplyOutcome::Deferred);
        }
        self.ensure_owned().await?;
        let changed = match user_config {
            Some(user) => self.write_daemon_config(user).await?,
            None => self.write_managed_keys().await?,
        };
        if !changed {
            return Ok(ProxyApplyOutcome::Unchanged);
        }
        self.stop_engine().await;
        self.start_engine().await?;
        Ok(ProxyApplyOutcome::Restarted)
    }

    /// The managed `daemon.json` for the settings editor, without the keys
    /// owned by the proxy and network settings.
    pub async fn read_daemon_config(&self) -> Result<String, RuntimeHostError> {
        if !self.session.is_running() {
            return Err(RuntimeHostError::State(
                t!("runtime_host.error.engine_not_running_daemon_config").into_owned(),
            ));
        }
        self.ensure_owned().await?;
        let current = self.read_daemon_json().await?;
        editable_daemon_config(&current).map_err(RuntimeHostError::State)
    }

    async fn read_daemon_json(&self) -> Result<String, RuntimeHostError> {
        let mut read = managed_command("root");
        read.arg("/bin/sh")
            .arg("-c")
            .arg("cat /etc/docker/daemon.json 2>/dev/null || true");
        let current = execute(
            read,
            t!("runtime_host.action.read_daemon_json"),
            COMMAND_TIMEOUT,
        )
        .await?;
        Ok(decode(&current.stdout))
    }

    /// Replaces `daemon.json` with the user's config plus the desired
    /// managed keys, after `dockerd --validate`. Returns whether it changed.
    async fn write_daemon_config(
        &self,
        user: Map<String, Value>,
    ) -> Result<bool, RuntimeHostError> {
        let mut document = user;
        document.extend(self.desired_managed());
        let document = Value::Object(document);
        let current = self.read_daemon_json().await?;
        if serde_json::from_str::<Value>(&current).ok().as_ref() == Some(&document)
            || current.trim().is_empty() && document == Value::Object(Map::new())
        {
            return Ok(false);
        }
        let text = serde_json::to_string_pretty(&document)
            .map_err(|error| RuntimeHostError::State(error.to_string()))?;
        let mut write = managed_command("root");
        write
            .arg("/bin/sh")
            .arg("-c")
            .arg(VALIDATE_AND_WRITE_DAEMON_JSON_SCRIPT);
        execute_with_input(
            write,
            format!("{text}\n").as_bytes(),
            t!("runtime_host.action.validate_and_write_daemon_json"),
            COMMAND_TIMEOUT,
        )
        .await?;
        Ok(true)
    }

    /// Merges the desired managed keys (proxy, network) into the managed
    /// `daemon.json`, keeping every other key (e.g. the NVIDIA runtime).
    /// Returns whether it changed.
    async fn write_managed_keys(&self) -> Result<bool, RuntimeHostError> {
        let current = self.read_daemon_json().await?;
        let Some(updated) = merge_managed_keys(&current, &self.desired_managed())
            .map_err(RuntimeHostError::State)?
        else {
            return Ok(false);
        };
        let mut write = managed_command("root");
        write
            .arg("/bin/sh")
            .arg("-c")
            .arg(VALIDATE_AND_WRITE_DAEMON_JSON_SCRIPT);
        execute_with_input(
            write,
            updated.as_bytes(),
            t!("runtime_host.action.write_daemon_engine_config"),
            COMMAND_TIMEOUT,
        )
        .await?;
        Ok(true)
    }

    pub fn lan_forward_status(&self) -> Arc<LanForwardStatus> {
        self.lan_forward.clone()
    }

    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) fn proxy_status(&self) -> Arc<ProxyStatus> {
        self.proxy.clone()
    }

    pub async fn snapshot(
        &self,
        integrations: &BTreeMap<String, bool>,
    ) -> Result<RuntimeHostSnapshot, RuntimeHostError> {
        ensure_windows()?;
        let distros = list_distributions().await?;
        let managed = distros
            .iter()
            .find(|distribution| distribution.name == MANAGED_DISTRO_NAME);
        if managed.is_some() && !self.receipt_valid() {
            return Err(RuntimeHostError::Conflict(
                t!(
                    "runtime_host.error.unmanaged_distro_conflict",
                    name = MANAGED_DISTRO_NAME
                )
                .into_owned(),
            ));
        }
        let installed = managed.is_some();
        let running = managed
            .map(|distribution| distribution.state.eq_ignore_ascii_case("running"))
            .unwrap_or(false);
        let session_alive = self.session.is_running();
        let paused = self.saver.is_active();
        let engine_ready =
            installed && running && session_alive && !paused && self.probe_engine().await.is_ok();
        let gpu = if engine_ready {
            probe_gpu().await
        } else {
            NvidiaGpuStatus {
                detail: t!("runtime_host.error.engine_not_running").into_owned(),
                ..NvidiaGpuStatus::default()
            }
        };
        let mut warnings = Vec::new();
        if let Some(error) = self.proxy.error() {
            warnings.push(error);
        }
        if let Some(error) = self.engine_proxy.error().filter(|_| session_alive) {
            warnings.push(error);
        }
        if installed && running && !engine_ready && !paused {
            warnings.push(if session_alive {
                t!("runtime_host.warning.engine_not_ready").into_owned()
            } else {
                t!("runtime_host.warning.distro_running_no_session").into_owned()
            });
        }
        // Only asked while running: any `wsl.exe -d` would boot the VM.
        if installed && running {
            if let Some(version) = image_version().await.filter(|v| v != ROOTFS_IMAGE) {
                warnings.push(if version.is_empty() {
                    t!(
                        "runtime_host.warning.old_ubuntu_environment",
                        image = ROOTFS_IMAGE
                    )
                    .into_owned()
                } else {
                    t!(
                        "runtime_host.warning.image_version_outdated",
                        version = version,
                        image = ROOTFS_IMAGE
                    )
                    .into_owned()
                });
            }
        }
        let distributions = distros
            .into_iter()
            .filter(|distribution| {
                distribution.version == 2
                    && distribution.name != MANAGED_DISTRO_NAME
                    && !matches!(
                        distribution.name.as_str(),
                        "docker-desktop" | "docker-desktop-data"
                    )
            })
            .map(|distribution| WslDistribution {
                integration_enabled: integrations
                    .get(&distribution.name)
                    .copied()
                    .unwrap_or(false),
                name: distribution.name,
                state: distribution.state,
                is_default: distribution.is_default,
            })
            .collect();
        let disk_location = if installed {
            self.data_disk_location()
        } else {
            None
        };
        Ok(RuntimeHostSnapshot {
            supported: true,
            installed,
            running,
            engine_ready,
            gpu,
            distributions,
            warnings,
            resource_saver_active: paused,
            disk_location,
        })
    }

    /// Installs, repairs or upgrades the managed environment, like Docker
    /// Desktop: the system disk is always (re)created from the pinned Rusker
    /// system image, while containers, images and volumes stay on the data
    /// disk and the user's `daemon.json` is carried over.
    pub async fn install(&self) -> Result<(), RuntimeHostError> {
        ensure_windows()?;
        let exists = list_distributions()
            .await?
            .iter()
            .any(|distribution| distribution.name == MANAGED_DISTRO_NAME);
        if exists {
            self.ensure_owned().await?;
        }
        // Find the bundled image before touching the existing environment.
        let rootfs = self.bundled_rootfs()?;
        if exists {
            self.retire_system_disk().await?;
        }

        let install_dir = self.distro_location();
        prepare_empty_directory(&install_dir)?;
        let mut command = wsl_command();
        command
            .arg("--import")
            .arg(MANAGED_DISTRO_NAME)
            .arg(&install_dir)
            .arg(&rootfs)
            .args(["--version", "2"]);
        if let Err(error) = execute(
            command,
            t!("runtime_host.action.import_wsl_distro"),
            INSTALL_TIMEOUT,
        )
        .await
        {
            let _ = fs::remove_dir_all(&install_dir);
            return Err(error);
        }
        if let Err(error) = self.write_receipt() {
            let mut unregister = wsl_command();
            unregister.args(["--unregister", MANAGED_DISTRO_NAME]);
            let _ = execute(
                unregister,
                t!("runtime_host.action.rollback_unregistered_distro"),
                COMMAND_TIMEOUT,
            )
            .await;
            return Err(error);
        }

        let mut verify = managed_command("root");
        verify
            .arg("/bin/sh")
            .arg("-c")
            .arg(VERIFY_IMAGE_SCRIPT)
            .arg("rusker-verify")
            .arg(ROOTFS_IMAGE);
        execute(
            verify,
            t!("runtime_host.action.verify_system_image"),
            COMMAND_TIMEOUT,
        )
        .await?;
        self.ensure_owned().await?;
        let restore_problem = self.restore_daemon_config().await?;
        let mut terminate = wsl_command();
        terminate.args(["--terminate", MANAGED_DISTRO_NAME]);
        execute(
            terminate,
            t!("runtime_host.action.apply_distro_config"),
            COMMAND_TIMEOUT,
        )
        .await?;
        self.start_engine().await?;
        match restore_problem {
            Some(message) => Err(RuntimeHostError::State(message)),
            None => Ok(()),
        }
    }

    /// Unregisters the current system disk after making sure Docker's state
    /// is on the data disk and saving `daemon.json` next to the receipt.
    async fn retire_system_disk(&self) -> Result<(), RuntimeHostError> {
        self.engine_wanted.store(false, Ordering::Relaxed);
        self.saver.set_active(false);
        self.stop_engine().await;
        // Also migrates an older install that still kept /var/lib/docker on
        // the system disk.
        self.attach_data_disk().await?;
        let mut check = managed_command("root");
        check.arg("/bin/sh").arg("-c").arg(DATA_ON_DATA_DISK_SCRIPT);
        execute(
            check,
            t!("runtime_host.action.verify_data_on_data_disk"),
            COMMAND_TIMEOUT,
        )
        .await
        .map_err(|error| {
            RuntimeHostError::State(
                t!("runtime_host.error.data_not_migrated", error = error).into_owned(),
            )
        })?;
        let config = self.read_daemon_json().await?;
        // A file still pending from an interrupted install is the user's
        // config; the current one may already be the image default.
        let preserved = self.preserved_config_path();
        if !config.trim().is_empty() && !preserved.exists() {
            fs::write(&preserved, config).map_err(|source| RuntimeHostError::Io {
                action: t!("runtime_host.action.save_daemon_json").into_owned(),
                source,
            })?;
        }
        let mut terminate = wsl_command();
        terminate.args(["--terminate", MANAGED_DISTRO_NAME]);
        execute(
            terminate,
            t!("runtime_host.action.stop_wsl_environment"),
            COMMAND_TIMEOUT,
        )
        .await?;
        self.detach_data_disk().await;
        let mut unregister = wsl_command();
        unregister.args(["--unregister", MANAGED_DISTRO_NAME]);
        execute(
            unregister,
            t!("runtime_host.action.remove_old_system_disk"),
            INSTALL_TIMEOUT,
        )
        .await?;
        Ok(())
    }

    /// Puts back the `daemon.json` saved by [`Self::retire_system_disk`],
    /// keeping the image's runtimes (the NVIDIA runtime) when the saved file
    /// lacks them. A file the new dockerd rejects is set aside instead of
    /// blocking the install; the message describing that is returned.
    async fn restore_daemon_config(&self) -> Result<Option<String>, RuntimeHostError> {
        let path = self.preserved_config_path();
        let Ok(saved) = fs::read_to_string(&path) else {
            return Ok(None);
        };
        let image = self.read_daemon_json().await?;
        let result = match restored_daemon_config(&saved, &image) {
            Ok(merged) => {
                let mut write = managed_command("root");
                write
                    .arg("/bin/sh")
                    .arg("-c")
                    .arg(VALIDATE_AND_WRITE_DAEMON_JSON_SCRIPT);
                execute_with_input(
                    write,
                    merged.as_bytes(),
                    t!("runtime_host.action.restore_daemon_json"),
                    COMMAND_TIMEOUT,
                )
                .await
                .map(|_| ())
                .map_err(|error| error.to_string())
            }
            Err(message) => Err(message),
        };
        match result {
            Ok(()) => {
                let _ = fs::remove_file(&path);
                Ok(None)
            }
            Err(message) => {
                let rejected = self.root.join("daemon.json.rejected");
                let _ = fs::rename(&path, &rejected);
                Ok(Some(
                    t!(
                        "runtime_host.error.daemon_config_rejected",
                        path = rejected.display(),
                        message = message
                    )
                    .into_owned(),
                ))
            }
        }
    }

    fn preserved_config_path(&self) -> PathBuf {
        self.root.join("daemon.json.preserved")
    }

    pub async fn start(&self) -> Result<(), RuntimeHostError> {
        if self.saver.is_active() && self.session.is_running() {
            return self.resume_resource_saver().await;
        }
        self.ensure_owned().await?;
        self.start_engine().await
    }

    /// Launch-time setup: install a missing managed environment, or start an
    /// existing engine, then restore saved integrations.
    pub async fn startup(
        &self,
        integrations: &BTreeMap<String, bool>,
    ) -> Result<RuntimeHostSnapshot, RuntimeHostError> {
        self.saver.set_integrations(integrations);
        let snapshot = self.snapshot(integrations).await?;
        if snapshot.engine_ready {
            return Ok(snapshot);
        }
        if snapshot.installed {
            self.start().await?;
        } else {
            self.install().await?;
        }
        self.snapshot_after_integration(integrations).await
    }

    pub async fn stop(&self) -> Result<(), RuntimeHostError> {
        self.ensure_owned().await?;
        // A manual stop is not Resource Saver: clients must not wake it.
        self.saver.set_active(false);
        self.engine_wanted.store(false, Ordering::Relaxed);
        self.stop_engine().await;
        let mut command = wsl_command();
        command.args(["--terminate", MANAGED_DISTRO_NAME]);
        execute(
            command,
            t!("runtime_host.action.stop_wsl_environment"),
            COMMAND_TIMEOUT,
        )
        .await?;
        self.detach_data_disk().await;
        Ok(())
    }

    /// Graceful shutdown on application exit, like Docker Desktop: stops an
    /// engine whose session this process holds, then terminates the managed
    /// distribution. Relying on WSL's idle timeout is not enough — leftover
    /// processes (containerd shims, systemd, …) keep it `Running` and holding
    /// memory indefinitely.
    pub async fn shutdown(&self) {
        self.engine_wanted.store(false, Ordering::Relaxed);
        if !self.session.is_running() {
            return;
        }
        self.stop_engine().await;
        if self.receipt_valid() {
            let mut terminate = wsl_command();
            terminate.args(["--terminate", MANAGED_DISTRO_NAME]);
            let _ = execute(
                terminate,
                t!("runtime_host.action.stop_wsl_environment"),
                COMMAND_TIMEOUT,
            )
            .await;
            self.detach_data_disk().await;
        }
    }

    pub async fn snapshot_after_integration(
        &self,
        integrations: &BTreeMap<String, bool>,
    ) -> Result<RuntimeHostSnapshot, RuntimeHostError> {
        self.saver.set_integrations(integrations);
        let mut warnings = Vec::new();
        for distribution in integrations
            .iter()
            .filter(|(_, enabled)| **enabled)
            .map(|(name, _)| name)
        {
            if let Err(error) = self.set_integration(distribution, true).await {
                warnings.push(
                    t!(
                        "runtime_host.error.restore_integration_failed",
                        distribution = distribution,
                        error = error
                    )
                    .into_owned(),
                );
            }
        }
        let mut snapshot = self.snapshot(integrations).await?;
        snapshot.warnings.extend(warnings);
        Ok(snapshot)
    }

    pub async fn set_integration(
        &self,
        distribution: &str,
        enabled: bool,
    ) -> Result<(), RuntimeHostError> {
        if enabled {
            self.resume_resource_saver().await?;
        }
        self.ensure_owned().await?;
        let distros = list_distributions().await?;
        let Some(target) = distros.iter().find(|candidate| {
            candidate.name == distribution && candidate.name != MANAGED_DISTRO_NAME
        }) else {
            return Err(RuntimeHostError::Conflict(
                t!(
                    "runtime_host.error.distribution_not_found",
                    distribution = distribution
                )
                .into_owned(),
            ));
        };
        if target.version != 2 {
            return Err(RuntimeHostError::Conflict(
                t!(
                    "runtime_host.error.not_wsl2_distribution",
                    distribution = distribution
                )
                .into_owned(),
            ));
        }
        if enabled {
            self.probe_engine()
                .await
                .map_err(|message| RuntimeHostError::Command {
                    action: t!("runtime_host.action.check_managed_engine").into_owned(),
                    message,
                })?;
            self.copy_managed_cli(distribution).await
        } else {
            let mut command = wsl_command_for(distribution, "root");
            command.arg("/bin/sh").arg("-c").arg(REMOVE_CLI_SCRIPT);
            execute(
                command,
                t!(
                    "runtime_host.action.disable_integration",
                    distribution = distribution
                ),
                COMMAND_TIMEOUT,
            )
            .await?;
            Ok(())
        }
    }

    async fn ensure_owned(&self) -> Result<(), RuntimeHostError> {
        if !self.receipt_valid() {
            return Err(RuntimeHostError::Conflict(
                t!("runtime_host.error.no_ownership_record").into_owned(),
            ));
        }
        let mut command = managed_command("root");
        command.args(["/usr/bin/test", "-f", MANAGED_MARKER]);
        execute(
            command,
            t!("runtime_host.action.verify_distro_marker"),
            COMMAND_TIMEOUT,
        )
        .await?;
        Ok(())
    }

    async fn start_engine(&self) -> Result<(), RuntimeHostError> {
        if !self.session.is_running() {
            let wwid = self.attach_data_disk().await?;
            // A previous app instance may have exited while the daemon was
            // paused. Resume recorded identities BEFORE sending any SIGTERM
            // for a config change; a frozen daemon cannot handle that signal.
            self.continue_engine().await?;
            // Write the current proxy and network config before dockerd
            // reads it. If it changed while a leftover dockerd is still
            // running, stop that one so the supervisor starts a fresh daemon
            // instead of attaching to a stale configuration.
            if self.write_managed_keys().await? {
                let mut stop = managed_command("root");
                stop.arg("/bin/sh").arg("-c").arg(STOP_SCRIPT);
                let _ = execute(
                    stop,
                    t!("runtime_host.action.stop_old_dockerd"),
                    DOCKERD_STOP_TIMEOUT,
                )
                .await;
            }
            let mut command = managed_command("root");
            command
                .arg("/bin/sh")
                .arg("-c")
                .arg(rendered_supervisor_script())
                .arg("rusker-supervisor")
                .arg(&wwid)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            let child = command
                .spawn()
                .map_err(|source| RuntimeHostError::Command {
                    action: t!("runtime_host.action.start_managed_engine_session").into_owned(),
                    message: source.to_string(),
                })?;
            self.session.replace(child);
        }
        self.wait_until_ready().await?;
        self.saver.set_active(false);
        // From now on a vanished session is an accident to recover from.
        self.engine_wanted.store(true, Ordering::Relaxed);
        self.recovery.clear_failure();
        Ok(())
    }

    /// SIGTERM dockerd (so containers stop cleanly), then release the session.
    async fn stop_engine(&self) {
        // A frozen daemon cannot handle SIGTERM. Also recovers a pause whose
        // command failed after writing the Linux-side identity file.
        let _ = self.continue_engine().await;
        self.saver.set_active(false);
        let mut command = managed_command("root");
        command.arg("/bin/sh").arg("-c").arg(STOP_SCRIPT);
        let _ = execute(
            command,
            t!("runtime_host.action.stop_dockerd"),
            DOCKERD_STOP_TIMEOUT,
        )
        .await;
        if let Some(mut child) = self.session.take() {
            if timeout(Duration::from_secs(5), child.wait()).await.is_err() {
                let _ = child.start_kill();
                let _ = child.wait().await;
            }
        }
    }

    /// Private stdio probe bypasses the public proxy: readiness is checked
    /// while holding the lifecycle lock, which public clients must wait on.
    async fn probe_engine(&self) -> Result<(), String> {
        let mut child = dial_stdio_command()
            .spawn()
            .map_err(|error| error.to_string())?;
        let mut stdin = child.stdin.take().ok_or("ping stdin unavailable")?;
        let mut stdout = child.stdout.take().ok_or("ping stdout unavailable")?;
        let result = timeout(Duration::from_secs(2), async {
            stdin
                .write_all(b"GET /_ping HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
                .await
                .map_err(|error| error.to_string())?;
            let mut response = Vec::new();
            let mut buffer = [0u8; 512];
            loop {
                let count = stdout
                    .read(&mut buffer)
                    .await
                    .map_err(|error| error.to_string())?;
                if count == 0 {
                    return Err("Engine ping connection closed".into());
                }
                response.extend_from_slice(&buffer[..count]);
                if response.windows(4).any(|part| part == b"\r\n\r\n") {
                    return if response.starts_with(b"HTTP/1.1 200 ")
                        || response.starts_with(b"HTTP/1.0 200 ")
                    {
                        Ok(())
                    } else {
                        Err(String::from_utf8_lossy(&response).into_owned())
                    };
                }
                if response.len() > 8192 {
                    return Err("Engine ping response headers too large".into());
                }
            }
        })
        .await
        .map_err(|_| "Engine ping timeout".to_string())
        .and_then(|result| result);
        let _ = child.start_kill();
        let _ = child.wait().await;
        result
    }

    async fn wait_until_ready(&self) -> Result<(), RuntimeHostError> {
        let started = std::time::Instant::now();
        let mut last_error = String::new();
        while started.elapsed() < READY_TIMEOUT {
            if !self.session.is_running() {
                return Err(RuntimeHostError::Command {
                    action: t!("runtime_host.action.start_managed_engine").into_owned(),
                    message: t!(
                        "runtime_host.error.engine_session_exited",
                        log = engine_log_tail().await
                    )
                    .into_owned(),
                });
            }
            match self.probe_engine().await {
                Ok(()) => return Ok(()),
                Err(error) => last_error = error,
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        Err(RuntimeHostError::Command {
            action: t!("runtime_host.action.wait_engine_ready").into_owned(),
            message: t!(
                "runtime_host.error.engine_ready_with_log",
                error = last_error,
                log = engine_log_tail().await
            )
            .into_owned(),
        })
    }

    /// Copies the managed Docker CLI (plus Compose/Buildx plugins) into the
    /// target distribution, streaming a tar archive between the two
    /// `wsl.exe` processes. Skipped when the installed fingerprint matches.
    async fn copy_managed_cli(&self, distribution: &str) -> Result<(), RuntimeHostError> {
        let mut fingerprint = managed_command("root");
        fingerprint
            .arg("/bin/sh")
            .arg("-c")
            .arg(MANAGED_CLI_SCRIPT)
            .args(["rusker-cli", "fingerprint"]);
        let fingerprint = decode(
            &execute(
                fingerprint,
                t!("runtime_host.action.compute_cli_fingerprint"),
                COMMAND_TIMEOUT,
            )
            .await?
            .stdout,
        )
        .trim()
        .to_string();
        if fingerprint.len() != 64 {
            return Err(RuntimeHostError::State(
                t!(
                    "runtime_host.error.invalid_cli_fingerprint",
                    fingerprint = fingerprint
                )
                .into_owned(),
            ));
        }

        let mut state = wsl_command_for(distribution, "root");
        state.arg("/bin/sh").arg("-c").arg(INTEGRATION_STATE_SCRIPT);
        let installed = execute(
            state,
            t!(
                "runtime_host.action.check_integration_state",
                distribution = distribution
            ),
            COMMAND_TIMEOUT,
        )
        .await
        .map(|output| decode(&output.stdout).trim().to_string())
        .unwrap_or_default();
        if installed == fingerprint {
            return Ok(());
        }

        let mut source = managed_command("root");
        source
            .arg("/bin/sh")
            .arg("-c")
            .arg(MANAGED_CLI_SCRIPT)
            .args(["rusker-cli", "archive"]);
        let mut target = wsl_command_for(distribution, "root");
        target
            .arg("/bin/sh")
            .arg("-c")
            .arg(INSTALL_CLI_SCRIPT)
            .arg("rusker-integration")
            .arg(&fingerprint);
        stream_between(
            source,
            target,
            t!(
                "runtime_host.action.enable_integration",
                distribution = distribution
            ),
            TRANSFER_TIMEOUT,
        )
        .await
    }

    fn receipt_path(&self) -> PathBuf {
        self.root.join("managed-distro")
    }

    fn receipt_valid(&self) -> bool {
        fs::read_to_string(self.receipt_path())
            .map(|content| content == MANAGED_RECEIPT)
            .unwrap_or(false)
    }

    fn write_receipt(&self) -> Result<(), RuntimeHostError> {
        fs::create_dir_all(&self.root).map_err(|source| RuntimeHostError::Io {
            action: t!("runtime_host.action.create_runtime_data_dir").into_owned(),
            source,
        })?;
        fs::write(self.receipt_path(), MANAGED_RECEIPT).map_err(|source| RuntimeHostError::Io {
            action: t!("runtime_host.action.write_ownership_record").into_owned(),
            source,
        })
    }

    /// The system image the Windows installer put next to the executable.
    /// It is never downloaded: without it the installation is incomplete.
    fn bundled_rootfs(&self) -> Result<PathBuf, RuntimeHostError> {
        // Older versions downloaded an Ubuntu cloud image into this cache.
        let _ = fs::remove_dir_all(self.root.join("rootfs"));
        let filename = rootfs_filename(std::env::consts::ARCH)?;
        let exe = std::env::current_exe().map_err(|source| RuntimeHostError::Io {
            action: t!("runtime_host.action.locate_app_directory").into_owned(),
            source,
        })?;
        let path = exe
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(BUNDLED_ROOTFS_DIR)
            .join(filename);
        if path.is_file() {
            Ok(path)
        } else {
            Err(RuntimeHostError::State(
                t!(
                    "runtime_host.error.system_image_missing",
                    path = path.display()
                )
                .into_owned(),
            ))
        }
    }
}

#[derive(Debug)]
struct DistroInfo {
    name: String,
    state: String,
    is_default: bool,
    version: u8,
}

async fn list_distributions() -> Result<Vec<DistroInfo>, RuntimeHostError> {
    let mut command = wsl_command();
    command.args(["--list", "--verbose"]);
    let output = execute(
        command,
        t!("runtime_host.action.list_wsl_distributions"),
        COMMAND_TIMEOUT,
    )
    .await?;
    Ok(parse_distributions(&decode(&output.stdout)))
}

fn parse_distributions(output: &str) -> Vec<DistroInfo> {
    output
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with("NAME") {
                return None;
            }
            let is_default = line.starts_with('*');
            let columns: Vec<&str> = line
                .split_whitespace()
                .filter(|column| *column != "*")
                .collect();
            if columns.len() < 3 {
                return None;
            }
            let version = columns.last()?.parse::<u8>().ok()?;
            let name = columns[..columns.len() - 2].join(" ");
            (!name.is_empty()).then(|| DistroInfo {
                name,
                state: columns[columns.len() - 2].to_string(),
                is_default,
                version,
            })
        })
        .collect()
}

pub(super) fn managed_command(user: &str) -> Command {
    wsl_command_for(MANAGED_DISTRO_NAME, user)
}

fn wsl_command_for(distribution: &str, user: &str) -> Command {
    let mut command = wsl_command();
    command
        .arg("--distribution")
        .arg(distribution)
        .arg("--user")
        .arg(user)
        .arg("--exec");
    command
}

fn wsl_command() -> Command {
    let mut command = Command::new("wsl.exe");
    command.kill_on_drop(true);
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Windows paths compare case-insensitively and ignore trailing separators
/// and the `\\?\` verbatim prefix.
pub fn same_path(left: &Path, right: &Path) -> bool {
    fn key(path: &Path) -> String {
        strip_verbatim(&path.to_string_lossy())
            .trim_end_matches(['\\', '/'])
            .replace('/', "\\")
            .to_lowercase()
    }
    key(left) == key(right)
}

fn strip_verbatim(path: &str) -> &str {
    path.strip_prefix(r"\\?\").unwrap_or(path)
}

pub(super) async fn execute(
    mut command: Command,
    action: impl Into<String>,
    duration: Duration,
) -> Result<Output, RuntimeHostError> {
    let action = action.into();
    match timeout(duration, command.output()).await {
        Ok(Ok(output)) => check_output(output, &action),
        Ok(Err(source)) => Err(RuntimeHostError::Command {
            action,
            message: source.to_string(),
        }),
        Err(_) => Err(RuntimeHostError::Timeout {
            action,
            seconds: duration.as_secs(),
        }),
    }
}

/// `daemon.json` keys the app writes from its settings: hidden from the
/// settings editor and refused there.
const MANAGED_DAEMON_KEYS: [&str; 6] = [
    "proxies",
    ENGINE_NETWORK_KEYS[0],
    ENGINE_NETWORK_KEYS[1],
    ENGINE_NETWORK_KEYS[2],
    ENGINE_NETWORK_KEYS[3],
    ENGINE_NETWORK_KEYS[4],
];

/// `daemon.json` as shown in the settings editor: pretty-printed, without
/// the managed keys. A missing file reads as `{}`.
fn editable_daemon_config(current: &str) -> Result<String, String> {
    let mut document = if current.trim().is_empty() {
        Map::new()
    } else {
        match serde_json::from_str::<Value>(current) {
            Ok(Value::Object(object)) => object,
            Ok(_) => return Err(t!("runtime_host.error.daemon_json_not_object").into_owned()),
            Err(error) => {
                return Err(
                    t!("runtime_host.error.daemon_json_invalid", error = error).into_owned()
                );
            }
        }
    };
    for key in MANAGED_DAEMON_KEYS {
        document.remove(key);
    }
    serde_json::to_string_pretty(&Value::Object(document)).map_err(|error| error.to_string())
}

/// Checks the settings editor's `daemon.json` text before it is applied
/// (`dockerd --validate` still runs on the engine side).
pub fn validate_daemon_config(text: &str) -> Result<(), String> {
    parse_editable_daemon_config(text).map(|_| ())
}

/// Parses the editor's text. Keys the app sets itself are refused, since
/// they would conflict at dockerd start.
fn parse_editable_daemon_config(text: &str) -> Result<Map<String, Value>, String> {
    let document = match serde_json::from_str::<Value>(text) {
        Ok(Value::Object(object)) => object,
        Ok(_) => return Err(t!("runtime_host.error.daemon_json_must_be_object").into_owned()),
        Err(error) => {
            return Err(t!(
                "runtime_host.error.daemon_json_not_valid_json",
                error = error
            )
            .into_owned());
        }
    };
    if document.contains_key("proxies") {
        return Err(t!("runtime_host.error.proxies_in_daemon_json").into_owned());
    }
    if let Some(key) = ENGINE_NETWORK_KEYS
        .iter()
        .find(|key| document.contains_key(**key))
    {
        return Err(t!("runtime_host.error.network_key_in_daemon_json", key = key).into_owned());
    }
    if document.contains_key("hosts") {
        return Err(t!("runtime_host.error.hosts_in_daemon_json").into_owned());
    }
    Ok(document)
}

/// Returns the new `daemon.json` text when a managed key must change.
/// Managed keys missing from `desired` are removed (an empty object counts
/// as missing). Refuses to touch a file that is not a JSON object, rather
/// than overwriting a user's broken config.
fn merge_managed_keys(
    current: &str,
    desired: &Map<String, Value>,
) -> Result<Option<String>, String> {
    let mut document = if current.trim().is_empty() {
        Map::new()
    } else {
        match serde_json::from_str::<Value>(current) {
            Ok(Value::Object(object)) => object,
            Ok(_) => {
                return Err(t!("runtime_host.error.daemon_json_not_object_unchanged").into_owned());
            }
            Err(error) => {
                return Err(t!(
                    "runtime_host.error.daemon_json_invalid_unchanged",
                    error = error
                )
                .into_owned());
            }
        }
    };
    let effective = |value: Option<&Value>| {
        value
            .filter(|value| !matches!(value, Value::Object(object) if object.is_empty()))
            .cloned()
    };
    let mut changed = false;
    for key in MANAGED_DAEMON_KEYS {
        let wanted = effective(desired.get(key));
        if effective(document.get(key)) == wanted {
            continue;
        }
        changed = true;
        match wanted {
            Some(value) => {
                document.insert(key.into(), value);
            }
            None => {
                document.remove(key);
            }
        }
    }
    if !changed {
        return Ok(None);
    }
    let text = serde_json::to_string_pretty(&Value::Object(document))
        .map_err(|error| error.to_string())?;
    Ok(Some(format!("{text}\n")))
}

pub(super) async fn execute_with_input(
    mut command: Command,
    input: &[u8],
    action: impl Into<String>,
    duration: Duration,
) -> Result<Output, RuntimeHostError> {
    let action = action.into();
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let failed = |source: std::io::Error| RuntimeHostError::Command {
        action: action.clone(),
        message: source.to_string(),
    };
    let operation = async {
        let mut child = command.spawn().map_err(failed)?;
        let mut stdin = child.stdin.take().ok_or_else(|| {
            RuntimeHostError::State(t!("runtime_host.error.cannot_open_command_stdin").into_owned())
        })?;
        stdin.write_all(input).await.map_err(failed)?;
        stdin.shutdown().await.map_err(failed)?;
        drop(stdin);
        child.wait_with_output().await.map_err(failed)
    };
    match timeout(duration, operation).await {
        Ok(Ok(output)) => check_output(output, &action),
        Ok(Err(error)) => Err(error),
        Err(_) => Err(RuntimeHostError::Timeout {
            action,
            seconds: duration.as_secs(),
        }),
    }
}

/// Pipes `source`'s stdout into `target`'s stdin without buffering the whole
/// payload in memory.
async fn stream_between(
    mut source: Command,
    mut target: Command,
    action: impl Into<String>,
    duration: Duration,
) -> Result<(), RuntimeHostError> {
    let action = action.into();
    source
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    target
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let spawn_error = |source: std::io::Error| RuntimeHostError::Command {
        action: action.clone(),
        message: source.to_string(),
    };
    let operation = async {
        let mut producer = source.spawn().map_err(spawn_error)?;
        let mut consumer = target.spawn().map_err(spawn_error)?;
        let (Some(mut output), Some(mut input)) = (producer.stdout.take(), consumer.stdin.take())
        else {
            return Err(RuntimeHostError::State(
                t!("runtime_host.error.cannot_connect_integration_stdio").into_owned(),
            ));
        };
        let copy = async move {
            let result = tokio::io::copy(&mut output, &mut input).await;
            let _ = input.shutdown().await;
            result
        };
        let (_, produced, consumed) = tokio::join!(
            copy,
            producer.wait_with_output(),
            consumer.wait_with_output()
        );
        // The consumer's error is the meaningful one (e.g. "refusing to
        // overwrite"); a producer failure is usually just a broken pipe.
        check_output(consumed.map_err(spawn_error)?, &action)?;
        check_output(produced.map_err(spawn_error)?, &action)?;
        Ok(())
    };
    match timeout(duration, operation).await {
        Ok(result) => result,
        Err(_) => Err(RuntimeHostError::Timeout {
            action,
            seconds: duration.as_secs(),
        }),
    }
}

fn check_output(output: Output, action: &str) -> Result<Output, RuntimeHostError> {
    if output.status.success() {
        return Ok(output);
    }
    let stderr = decode(&output.stderr);
    let stdout = decode(&output.stdout);
    let message = if stderr.trim().is_empty() {
        stdout.trim()
    } else {
        stderr.trim()
    };
    let message: String = message.chars().take(4096).collect();
    Err(RuntimeHostError::Command {
        action: action.into(),
        message: if message.is_empty() {
            t!(
                "runtime_host.error.command_exit_status",
                status = output.status
            )
            .into_owned()
        } else {
            t!(
                "runtime_host.error.command_exit_status_with_message",
                status = output.status,
                message = message
            )
            .into_owned()
        },
    })
}

/// One relay process per Windows pipe client: `docker system dial-stdio`
/// proxies stdio to `/var/run/docker.sock` and turns stdin EOF into a socket
/// half-close, which hijacked attach/exec streams need.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn dial_stdio_command() -> Command {
    let mut command = managed_command("root");
    command
        .args(["/usr/bin/docker", "system", "dial-stdio"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    command
}

async fn engine_log_tail() -> String {
    let mut command = managed_command("root");
    command.args(["/usr/bin/tail", "-n", "20", "/var/log/rusker-dockerd.log"]);
    match execute(
        command,
        t!("runtime_host.action.read_dockerd_log"),
        COMMAND_TIMEOUT,
    )
    .await
    {
        Ok(output) => decode(&output.stdout).trim().to_string(),
        Err(error) => error.to_string(),
    }
}

async fn probe_gpu() -> NvidiaGpuStatus {
    let mut probe = managed_command("root");
    probe.arg("/bin/sh").arg("-c").arg(GPU_PROBE_SCRIPT);
    let output = match execute(
        probe,
        t!("runtime_host.action.detect_nvidia_gpu"),
        COMMAND_TIMEOUT,
    )
    .await
    {
        Ok(output) => output,
        Err(error) => {
            return NvidiaGpuStatus {
                detail: error.to_string(),
                ..NvidiaGpuStatus::default()
            }
        }
    };
    let name = decode(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(", ");

    let mut runtimes = managed_command("root");
    runtimes.args(["/usr/bin/docker", "info", "--format", "{{json .Runtimes}}"]);
    let runtime_registered = execute(
        runtimes,
        t!("runtime_host.action.check_nvidia_docker_runtime"),
        COMMAND_TIMEOUT,
    )
    .await
    .ok()
    .and_then(|output| serde_json::from_slice::<serde_json::Value>(&output.stdout).ok())
    .and_then(|value| {
        value
            .as_object()
            .map(|object| object.contains_key("nvidia"))
    })
    .unwrap_or(false);
    if runtime_registered {
        NvidiaGpuStatus {
            available: true,
            name: (!name.is_empty()).then(|| name.clone()),
            detail: if name.is_empty() {
                t!("runtime_host.gpu.nvidia_runtime_ready").into_owned()
            } else {
                t!(
                    "runtime_host.gpu.nvidia_runtime_ready_with_name",
                    name = name
                )
                .into_owned()
            },
        }
    } else {
        NvidiaGpuStatus {
            detail: t!("runtime_host.gpu.nvidia_runtime_missing").into_owned(),
            name: (!name.is_empty()).then_some(name),
            available: false,
        }
    }
}

/// The managed distribution's system image version (empty for an install
/// from before the Rusker image), or None when it cannot be asked.
async fn image_version() -> Option<String> {
    let mut command = managed_command("root");
    command.arg("/bin/sh").arg("-c").arg(IMAGE_VERSION_SCRIPT);
    execute(
        command,
        t!("runtime_host.action.read_system_image_version"),
        COMMAND_TIMEOUT,
    )
    .await
    .ok()
    .map(|output| decode(&output.stdout).trim().to_string())
}

/// The supervisor with the data disk mount script inlined; `$1` is the WWID.
fn rendered_supervisor_script() -> String {
    SUPERVISOR_SCRIPT.replace("__MOUNT_DATA_DISK__", data_disk::MOUNT_SCRIPT)
}

fn prepare_empty_directory(path: &Path) -> Result<(), RuntimeHostError> {
    match fs::read_dir(path) {
        Ok(mut entries) => {
            if entries.next().is_some() {
                return Err(RuntimeHostError::Conflict(
                    t!(
                        "runtime_host.error.distro_dir_not_empty",
                        path = path.display()
                    )
                    .into_owned(),
                ));
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => {
            return Err(RuntimeHostError::Io {
                action: t!("runtime_host.action.check_distro_directory").into_owned(),
                source,
            })
        }
    }
    fs::create_dir_all(path).map_err(|source| RuntimeHostError::Io {
        action: t!("runtime_host.action.create_distro_directory").into_owned(),
        source,
    })
}

/// File name of the system image for the WSL2 VM's architecture (the
/// host's), as built by `images/wsl-rootfs/build.sh`.
fn rootfs_filename(architecture: &str) -> Result<String, RuntimeHostError> {
    let arch = match architecture {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => {
            return Err(RuntimeHostError::State(
                t!("runtime_host.error.unsupported_architecture", arch = other).into_owned(),
            ))
        }
    };
    Ok(format!("{ROOTFS_IMAGE}-{arch}.tar.xz"))
}

/// `daemon.json` to restore after replacing the system disk: the saved
/// file, plus any runtime from the new image's file (the NVIDIA runtime)
/// that the saved one lacks.
fn restored_daemon_config(saved: &str, image: &str) -> Result<String, String> {
    let Value::Object(mut saved) =
        serde_json::from_str::<Value>(saved).map_err(|error| error.to_string())?
    else {
        return Err(t!("runtime_host.error.daemon_json_must_be_object").into_owned());
    };
    if let Ok(Value::Object(image)) = serde_json::from_str::<Value>(image) {
        if let Some(Value::Object(image_runtimes)) = image.get("runtimes") {
            let runtimes = saved
                .entry("runtimes")
                .or_insert_with(|| Value::Object(Map::new()));
            if let Value::Object(runtimes) = runtimes {
                for (name, runtime) in image_runtimes {
                    runtimes
                        .entry(name.clone())
                        .or_insert_with(|| runtime.clone());
                }
            }
        }
    }
    serde_json::to_string_pretty(&Value::Object(saved))
        .map(|text| format!("{text}\n"))
        .map_err(|error| error.to_string())
}

fn ensure_windows() -> Result<(), RuntimeHostError> {
    if cfg!(windows) {
        Ok(())
    } else {
        Err(RuntimeHostError::Unsupported)
    }
}

fn decode(bytes: &[u8]) -> String {
    let looks_utf16 = bytes.starts_with(&[0xff, 0xfe])
        || bytes.len() >= 4
            && bytes.len().is_multiple_of(2)
            && bytes
                .iter()
                .skip(1)
                .step_by(2)
                .filter(|byte| **byte == 0)
                .count()
                >= bytes.len() / 4;
    if looks_utf16 {
        let units: Vec<u16> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u16::from_le_bytes(*pair))
            .collect();
        String::from_utf16_lossy(&units)
            .trim_start_matches('\u{feff}')
            .replace('\0', "")
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{NetworkingMode, PortBinding, ProxyMode};

    #[test]
    fn compares_windows_paths() {
        assert!(same_path(
            Path::new(r"\\?\D:\WSL\Rusker\"),
            Path::new("d:/wsl/rusker")
        ));
        assert!(!same_path(
            Path::new(r"D:\WSL\Rusker"),
            Path::new(r"D:\WSL")
        ));
    }

    #[test]
    fn resource_saver_tracks_open_connections() {
        let saver = Arc::new(ResourceSaver::default());
        assert!(!saver.enabled());
        saver.configure(&ResourceSaverSettings {
            enabled: true,
            threshold_secs: 120,
        });
        assert!(saver.enabled());
        assert_eq!(saver.threshold(), Duration::from_secs(120));
        let first = saver.track_connection();
        let second = saver.track_connection();
        assert_eq!(saver.connections(), 2);
        drop(first);
        drop(second);
        assert_eq!(saver.connections(), 0);
    }

    #[test]
    fn resource_saver_keeps_session_active_and_blocks_wsl_integration() {
        let saver = ResourceSaver::default();
        saver.configure(&ResourceSaverSettings {
            enabled: true,
            threshold_secs: 30,
        });
        assert!(saver.can_pause());
        saver.set_integrations(&BTreeMap::from([("Ubuntu".into(), true)]));
        assert!(!saver.can_pause());
        saver.set_integrations(&BTreeMap::from([("Ubuntu".into(), false)]));
        assert!(saver.can_pause());
        saver.set_active(true);
        saver.configure(&ResourceSaverSettings {
            enabled: false,
            threshold_secs: 30,
        });
        assert!(
            saver.is_active(),
            "disabling must not hide a daemon still awaiting resume"
        );
        assert!(!saver.can_pause());
    }

    #[test]
    fn pause_scripts_preserve_processes_mounts_and_validate_pid_identity() {
        assert!(PAUSE_SCRIPT.contains("--host=unix:///var/run/docker.sock ps -q"));
        assert!(PAUSE_SCRIPT.contains("kill -STOP"));
        assert!(PAUSE_SCRIPT.contains("trap 'resume_engine'"));
        assert!(ENGINE_SIGNALS_SCRIPT.contains("kill -CONT"));
        assert!(ENGINE_SIGNALS_SCRIPT.contains("[ \"$current\" = \"$started\" ]"));
        for forbidden in ["kill -TERM", "--terminate", "--unmount", "dockerd --host"] {
            assert!(!PAUSE_SCRIPT.contains(forbidden));
        }
    }

    /// Opt-in real WSL regression. Never runs on ordinary cargo test.
    #[cfg(windows)]
    #[test]
    fn resource_saver_resumes_original_requests_without_restarting_engine() {
        if std::env::var("RUSKER_WSL_RESOURCE_SAVER").as_deref() != Ok("1") {
            return;
        }
        use crate::{engine::EngineService, model::ContainerAction};
        assert!(
            std::env::var_os("DOCKER_HOST").is_none(),
            "unset DOCKER_HOST: this regression only targets the managed engine"
        );
        let service = super::super::RuntimeHostService::new().expect("runtime host");
        let engine = EngineService::new().expect("engine service");
        let client_runtime = tokio::runtime::Runtime::new().expect("client runtime");
        let outcome = client_runtime.block_on(async {
            service.startup(BTreeMap::new()).await.unwrap()?;
            let host = &service.host;
            let docker = bollard::Docker::connect_with_named_pipe(
                MANAGED_PIPE, 120, bollard::API_DEFAULT_VERSION,
            ).unwrap().negotiate_version().await.unwrap();
            assert_eq!(host.running_containers().await?, 0, "test requires an idle dedicated engine");
            let identity = || async {
                let mut cmd = managed_command("root");
                cmd.args(["/bin/sh", "-c", "pid=$(cat /var/run/docker.pid); cat /proc/$pid/stat; findmnt -n -o SOURCE /var/lib/docker"]);
                let output = execute(cmd, "engine identity", COMMAND_TIMEOUT).await?;
                let text = decode(&output.stdout);
                let fields: Vec<_> = text.lines().next().unwrap().split_whitespace().collect();
                Ok::<_, RuntimeHostError>((fields[0].to_string(), fields[21].to_string(), text.lines().last().unwrap().to_string()))
            };
            let before = identity().await?;
            // No network pull: import only the system image's static busybox.
            let image = format!("rusker-test/resource-saver:{}", std::process::id());
            let mut import = managed_command("root");
            import.args(["/bin/sh", "-c", "set -eu; d=$(mktemp -d); trap 'rm -rf \"$d\"' EXIT; mkdir -p \"$d/bin\"; cp /bin/busybox \"$d/bin/\"; tar -C \"$d\" -c . | docker --host=unix:///var/run/docker.sock import - \"$1\" >/dev/null", "test-import", &image]);
            execute(import, "import test image", COMMAND_TIMEOUT).await?;
            let created = docker.create_container(None::<bollard::query_parameters::CreateContainerOptions>, bollard::models::ContainerCreateBody {
                image: Some(image.clone()),
                cmd: Some(vec!["/bin/busybox".into(), "sleep".into(), "300".into()]),
                ..Default::default()
            }).await.unwrap();
            let result = async {
                for _ in 0..3 {
                    host.resource_saver().configure(&ResourceSaverSettings { enabled: true, threshold_secs: 30 });
                    {
                        let _guard = service.operation_lock.lock().await;
                        host.enter_resource_saver().await?;
                    }
                    assert!(service.resource_saver_active());
                    assert!(host.session_running(), "pause must preserve supervisor");
                    let paused = service.snapshot(BTreeMap::new()).await.unwrap()?;
                    assert!(paused.running && paused.resource_saver_active && !paused.engine_ready);
                    assert!(service.resource_saver_active(), "snapshot must not wake engine");
                    let started = std::time::Instant::now();
                    // Original mutation, plus concurrent requests during wake-up.
                    let (action, version, info) = tokio::join!(
                        engine.container_action(created.id.clone(), ContainerAction::Start),
                        docker.version(), docker.info(),
                    );
                    action.unwrap().unwrap();
                    version.unwrap();
                    info.unwrap();
                    assert!(docker.inspect_container(&created.id, None).await.unwrap().state.unwrap().running.unwrap());
                    assert!(!service.resource_saver_active());
                    assert_eq!(identity().await?, before, "PID, start time and data mount must survive");
                    eprintln!("Resource Saver -> container started: {:?}", started.elapsed());
                    engine.container_action(created.id.clone(), ContainerAction::Stop).await.unwrap().unwrap();
                }
                // Disabling Resource Saver must actively resume it.
                {
                    let _guard = service.operation_lock.lock().await;
                    host.enter_resource_saver().await?;
                }
                service.set_resource_saver(&ResourceSaverSettings { enabled: false, threshold_secs: 30 });
                tokio::time::timeout(Duration::from_secs(10), async {
                    while service.resource_saver_active() {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                }).await.expect("disabling Resource Saver must resume without a client request");
                Ok::<_, RuntimeHostError>(())
            }.await;
            docker.remove_container(&created.id, Some(bollard::query_parameters::RemoveContainerOptionsBuilder::default().force(true).build())).await.unwrap();
            docker.remove_image(&image, None::<bollard::query_parameters::RemoveImageOptions>, None).await.unwrap();
            result
        });
        service.shutdown_blocking(Duration::from_secs(50));
        outcome.expect("resource saver regression");
    }

    #[test]
    fn parses_utf16_distribution_list_with_spaces_in_names() {
        let text = "  NAME            STATE      VERSION\r\n* Ubuntu 24.04    Stopped    2\r\n  legacy          Running    1\r\n";
        let utf16: Vec<u8> = text
            .encode_utf16()
            .flat_map(|unit| unit.to_le_bytes())
            .collect();
        let distros = parse_distributions(&decode(&utf16));
        assert_eq!(distros.len(), 2);
        assert_eq!(distros[0].name, "Ubuntu 24.04");
        assert_eq!(distros[0].state, "Stopped");
        assert_eq!(distros[0].version, 2);
        assert!(distros[0].is_default);
        assert_eq!(distros[1].version, 1);
    }

    const INSTALL_SH: &str = include_str!("../../images/wsl-rootfs/install.sh");

    #[test]
    fn names_the_bundled_system_image_per_architecture() {
        assert_eq!(
            ROOTFS_IMAGE,
            format!(
                "rusker-rootfs-v{}",
                include_str!("../../images/wsl-rootfs/VERSION").trim()
            )
        );
        for (architecture, arch) in [("x86_64", "amd64"), ("aarch64", "arm64")] {
            assert_eq!(
                rootfs_filename(architecture).expect("支持的架构"),
                format!("{ROOTFS_IMAGE}-{arch}.tar.xz")
            );
        }
        assert!(rootfs_filename("unsupported").is_err());
        // The installer puts the image where the app looks for it.
        let installer = include_str!("../../packaging/windows/installer.nsi");
        assert!(installer.contains(&format!(
            "$INSTDIR\\{}",
            BUNDLED_ROOTFS_DIR.replace('/', "\\")
        )));
    }

    #[test]
    fn image_build_matches_expected_version_and_marker() {
        let dockerfile = include_str!("../../images/wsl-rootfs/Dockerfile");
        let build = include_str!("../../images/wsl-rootfs/build.sh");
        assert!(INSTALL_SH.contains("RUSKER_IMAGE=rusker-rootfs-v$IMAGE_VERSION"));
        assert!(build.contains("rusker-rootfs-v${IMAGE_VERSION}-${arch}.tar.xz"));
        assert!(dockerfile.contains("IMAGE_VERSION:?"));
        assert!(VERIFY_IMAGE_SCRIPT.contains("RUSKER_IMAGE=$1"));
        // The baked marker must match the receipt, or ensure_owned refuses
        // the freshly imported distribution.
        let marker = format!(
            "printf '{}' > {MANAGED_MARKER}",
            MANAGED_RECEIPT.replace('\n', "\\n")
        );
        assert!(INSTALL_SH.contains(&marker), "{marker}");
    }

    #[test]
    fn restores_saved_daemon_config_and_keeps_image_runtimes() {
        let saved = r#"{"debug":true,"runtimes":{"custom":{"path":"crun"}}}"#;
        let image = r#"{"runtimes":{"nvidia":{"args":[],"path":"nvidia-container-runtime"}}}"#;
        let merged: Value =
            serde_json::from_str(&restored_daemon_config(saved, image).expect("合并"))
                .expect("JSON");
        assert_eq!(merged["debug"], true);
        assert_eq!(merged["runtimes"]["custom"]["path"], "crun");
        assert_eq!(
            merged["runtimes"]["nvidia"]["path"],
            "nvidia-container-runtime"
        );
        assert!(restored_daemon_config("[]", image).is_err());
    }

    #[test]
    fn system_image_does_not_install_a_linux_display_driver() {
        assert!(INSTALL_SH.contains("\"nvidia-container-toolkit=$NVIDIA_TOOLKIT_VERSION\""));
        assert!(INSTALL_SH.contains("nvidia-ctk runtime configure --runtime=docker"));
        for forbidden in [
            "cuda-drivers",
            "nvidia-open",
            "nvidia-driver",
            "nvidia-kernel",
        ] {
            assert!(!INSTALL_SH.contains(forbidden));
        }
        for package in [
            "docker-compose-plugin",
            "docker-buildx-plugin",
            "containerd.io",
        ] {
            assert!(INSTALL_SH.contains(package));
        }
    }

    #[test]
    fn engine_is_not_exposed_over_tcp() {
        for script in [
            SUPERVISOR_SCRIPT,
            INSTALL_CLI_SCRIPT,
            INTEGRATION_STATE_SCRIPT,
        ] {
            assert!(!script.contains("tcp://"));
            assert!(!script.contains("23753"));
        }
        assert!(SUPERVISOR_SCRIPT.contains("--host=unix:///var/run/docker.sock"));
        assert!(SUPERVISOR_SCRIPT.contains("SHARED=/mnt/wsl/rusker"));
        assert!(
            WSL_DOCKER_HOST.ends_with("/mnt/wsl/rusker/docker.sock")
                && INSTALL_CLI_SCRIPT.contains(WSL_DOCKER_HOST)
                && INTEGRATION_STATE_SCRIPT.contains(WSL_DOCKER_HOST)
        );
        assert_eq!(
            MANAGED_DOCKER_HOST
                .trim_start_matches("npipe://")
                .replace('/', "\\"),
            MANAGED_PIPE
        );
    }

    #[test]
    fn supervisor_runs_dockerd_in_foreground_and_reattaches() {
        assert!(SUPERVISOR_SCRIPT.contains("exec /usr/bin/dockerd"));
        assert!(!SUPERVISOR_SCRIPT.contains("nohup"));
        assert!(SUPERVISOR_SCRIPT.contains("attaching to running dockerd"));
        assert!(STOP_SCRIPT.contains("kill -TERM"));
        // The data disk is mounted before dockerd can touch /var/lib/docker.
        let rendered = rendered_supervisor_script();
        assert!(!rendered.contains("__MOUNT_DATA_DISK__"));
        let mount = rendered.find("mount --bind").expect("mount");
        assert!(mount < rendered.find("exec /usr/bin/dockerd").expect("dockerd"));
    }

    #[test]
    fn recovery_status_counts_successful_restarts() {
        let status = RecoveryStatus::default();
        assert_eq!(status.get(), (EngineRecovery::Idle, 0));
        status.set(EngineRecovery::Restarting { attempt: 1 });
        assert_eq!(status.get(), (EngineRecovery::Restarting { attempt: 1 }, 0));
        status.recovered();
        assert_eq!(status.get(), (EngineRecovery::Idle, 1));

        // A manual start clears a failure without counting as a recovery,
        // and never hides a restart in progress.
        status.set(EngineRecovery::Failed("x".into()));
        status.clear_failure();
        assert_eq!(status.get(), (EngineRecovery::Idle, 1));
        status.set(EngineRecovery::Restarting { attempt: 2 });
        status.clear_failure();
        assert_eq!(status.get().0, EngineRecovery::Restarting { attempt: 2 });
    }

    #[test]
    fn daemon_config_editor_hides_and_refuses_managed_keys() {
        let current = r#"{"proxies":{"http-proxy":"http://a:1"},"runtimes":{"nvidia":{"path":"nvidia-container-runtime"}}}"#;
        let editable = editable_daemon_config(current).expect("读取");
        assert!(!editable.contains("proxies"));
        assert!(editable.contains("nvidia-container-runtime"));
        assert_eq!(editable_daemon_config("").expect("空文件"), "{}");
        assert!(editable_daemon_config("[]").is_err());

        let parsed = parse_editable_daemon_config(&editable).expect("解析");
        assert!(parsed.contains_key("runtimes"));
        assert!(parse_editable_daemon_config(r#"{"experimental":false}"#).is_ok());
        assert!(parse_editable_daemon_config("{broken").is_err());
        assert!(parse_editable_daemon_config("[]").is_err());
        assert!(parse_editable_daemon_config(r#"{"proxies":{}}"#).is_err());
        assert!(parse_editable_daemon_config(r#"{"hosts":["tcp://0.0.0.0:2375"]}"#).is_err());
        assert!(VALIDATE_AND_WRITE_DAEMON_JSON_SCRIPT.contains("dockerd --validate"));
    }

    #[test]
    fn merges_proxies_without_touching_other_daemon_keys() {
        let current = r#"{"runtimes":{"nvidia":{"path":"nvidia-container-runtime","args":[]}}}"#;
        let desired = DockerProxySettings {
            mode: ProxyMode::Manual,
            http_proxy: "http://127.0.0.1:7890".into(),
            https_proxy: "http://127.0.0.1:7890".into(),
            no_proxy: "localhost,127.0.0.1".into(),
        }
        .engine_proxies();
        let desired = Map::from_iter([("proxies".to_string(), Value::Object(desired))]);
        let merged = merge_managed_keys(current, &desired)
            .expect("合并")
            .expect("应有变化");
        let value: Value = serde_json::from_str(&merged).expect("JSON");
        assert_eq!(
            value["runtimes"]["nvidia"]["path"],
            "nvidia-container-runtime"
        );
        assert_eq!(value["proxies"]["http-proxy"], "http://127.0.0.1:7890");

        // Same config again: nothing to write, so no restart.
        assert_eq!(merge_managed_keys(&merged, &desired).expect("合并"), None);

        // Disabling removes only the proxies key.
        let disabled = merge_managed_keys(&merged, &Map::new())
            .expect("合并")
            .expect("应有变化");
        let value: Value = serde_json::from_str(&disabled).expect("JSON");
        assert!(value.get("proxies").is_none());
        assert!(value.get("runtimes").is_some());

        assert_eq!(merge_managed_keys("", &Map::new()).expect("空文件"), None);
        assert_eq!(
            merge_managed_keys(r#"{"proxies":{}}"#, &Map::new()).expect("空对象"),
            None
        );
        assert!(merge_managed_keys("{broken", &desired).is_err());
        assert!(merge_managed_keys("[]", &desired).is_err());
    }

    #[test]
    fn merges_network_keys_and_removes_stale_ones() {
        let current =
            r#"{"runtimes":{"nvidia":{"path":"nvidia-container-runtime"}},"ip":"0.0.0.0"}"#;
        let network = NetworkSettings {
            subnet: "192.168.65.0/24".into(),
            host_networking: false,
            port_binding: PortBinding::Open,
            mode: NetworkingMode::DualStack,
        };
        let desired = network.engine_config();
        let merged = merge_managed_keys(current, &desired)
            .expect("合并")
            .expect("应有变化");
        let value: Value = serde_json::from_str(&merged).expect("JSON");
        assert_eq!(value["bip"], "192.168.65.1/24");
        assert_eq!(value["ipv6"], true);
        // Open binding is Docker's default, so a stale `ip` is dropped.
        assert!(value.get("ip").is_none());
        assert!(value.get("runtimes").is_some());
        assert_eq!(merge_managed_keys(&merged, &desired).expect("合并"), None);

        // Back to defaults: every network key goes, the rest stays.
        let reset = merge_managed_keys(&merged, &NetworkSettings::default().engine_config())
            .expect("合并")
            .expect("应有变化");
        let value: Value = serde_json::from_str(&reset).expect("JSON");
        for key in ENGINE_NETWORK_KEYS {
            assert!(value.get(key).is_none(), "{key}");
        }
        assert!(value.get("runtimes").is_some());

        let editable = editable_daemon_config(&merged).expect("读取");
        assert!(!editable.contains("bip"));
        assert!(!editable.contains("default-network-opts"));
        assert!(parse_editable_daemon_config(r#"{"bip":"10.0.0.1/24"}"#).is_err());
        assert!(parse_editable_daemon_config(r#"{"ipv6":true}"#).is_err());
    }

    #[test]
    fn integration_disable_only_removes_marked_files() {
        assert!(REMOVE_CLI_SCRIPT.contains("RUSKER_MANAGED_DOCKER_WRAPPER"));
        assert!(REMOVE_CLI_SCRIPT.contains("/usr/local/lib/rusker/integrated"));
        assert!(!REMOVE_CLI_SCRIPT.contains("/usr/bin/docker"));
        assert!(!REMOVE_CLI_SCRIPT.contains("rm -rf"));
        // Plugin links are removed only when they still point at Rusker files.
        assert!(REMOVE_CLI_SCRIPT.contains("readlink"));
        assert!(INSTALL_CLI_SCRIPT.contains("保留发行版自带的"));
    }
}
