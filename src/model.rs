#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EngineInfo {
    pub name: String,
    pub version: String,
    pub api_version: String,
    pub operating_system: String,
    pub architecture: String,
    /// CPUs available to the engine (`docker info` NCPU); 0 when unknown.
    pub cpus: u32,
    /// Memory available to the engine in bytes (`docker info` MemTotal).
    pub memory_bytes: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EngineSnapshot {
    pub engine: EngineInfo,
    pub containers: Vec<ContainerSummary>,
    pub images: Vec<ImageSummary>,
    pub volumes: Vec<VolumeSummary>,
}

impl EngineSnapshot {
    /// Containers (running or not) that mount the named volume.
    pub fn volume_users(&self, volume: &str) -> Vec<&ContainerSummary> {
        self.containers
            .iter()
            .filter(|container| container.volumes.iter().any(|name| name == volume))
            .collect()
    }

    pub fn volume_in_use(&self, volume: &str) -> bool {
        self.containers
            .iter()
            .any(|container| container.volumes.iter().any(|name| name == volume))
    }

    pub fn running_containers(&self) -> usize {
        self.containers
            .iter()
            .filter(|container| container.is_running())
            .count()
    }

    pub fn stopped_containers(&self) -> usize {
        self.containers.len() - self.running_containers()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ContainerSummary {
    pub id: String,
    pub name: String,
    pub image: String,
    /// Full ID (`sha256:…`) of the image the container was created from.
    pub image_id: String,
    pub state: String,
    pub status: String,
    pub created: i64,
    pub ports: Vec<PortMapping>,
    /// Names of the named/anonymous volumes this container mounts.
    pub volumes: Vec<String>,
}

impl ContainerSummary {
    pub fn is_running(&self) -> bool {
        self.state.eq_ignore_ascii_case("running")
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PortMapping {
    pub host_ip: Option<String>,
    pub private_port: u16,
    pub public_port: Option<u16>,
    pub protocol: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CreateContainerRequest {
    pub image: String,
    pub name: Option<String>,
    pub command: Vec<String>,
    pub ports: Vec<CreatePortBinding>,
    pub nvidia_gpu: bool,
    /// `NAME=value` environment variables.
    pub env: Vec<String>,
    /// `source:/container/path` bind mounts; the source is a host path
    /// (Windows paths are translated by the engine proxy) or a volume name.
    pub binds: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CreatePortBinding {
    pub host_ip: String,
    pub host_port: u16,
    pub container_port: u16,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ImageSummary {
    pub id: String,
    pub tags: Vec<String>,
    /// `repo@sha256:…` references; name source for untagged pulled images.
    pub digests: Vec<String>,
    pub created: i64,
    pub size: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ImageDetail {
    pub id: String,
    pub architecture: String,
    pub os: String,
    /// Engine history order: newest layer first, including zero-size layers.
    pub layers: Vec<ImageLayer>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ImageLayer {
    pub id: String,
    pub created: i64,
    pub created_by: String,
    pub size: u64,
    pub comment: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VolumeSummary {
    pub name: String,
    pub driver: String,
    pub mountpoint: String,
    pub scope: String,
    /// Unix timestamp; 0 when the engine did not report it.
    pub created: i64,
    pub labels: Vec<(String, String)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VolumeEntryKind {
    Directory,
    File,
    Symlink,
    Other,
}

/// One entry of a directory inside a volume.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VolumeEntry {
    pub name: String,
    pub kind: VolumeEntryKind,
    /// File size, or the recursive disk usage for directories.
    pub size: u64,
    pub modified: i64,
    /// `ls -l` style mode string, e.g. `drwxr-xr-x`.
    pub mode: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContainerAction {
    Start,
    Stop,
    Restart,
    Remove,
}

impl ContainerAction {
    pub fn label(self) -> std::borrow::Cow<'static, str> {
        match self {
            Self::Start => t!("model.action.start"),
            Self::Stop => t!("model.action.stop"),
            Self::Restart => t!("model.action.restart"),
            Self::Remove => t!("model.action.remove"),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Revision(u64);

impl Revision {
    pub fn advance(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(1);
        self.0
    }

    pub fn is_current(self, candidate: u64) -> bool {
        self.0 == candidate
    }

    pub fn current(self) -> u64 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_counts_running_and_stopped_containers() {
        let snapshot = EngineSnapshot {
            containers: vec![
                ContainerSummary {
                    state: "running".into(),
                    ..ContainerSummary::default()
                },
                ContainerSummary {
                    state: "exited".into(),
                    ..ContainerSummary::default()
                },
                ContainerSummary {
                    state: "paused".into(),
                    ..ContainerSummary::default()
                },
            ],
            ..EngineSnapshot::default()
        };

        assert_eq!(snapshot.running_containers(), 1);
        assert_eq!(snapshot.stopped_containers(), 2);
    }

    #[test]
    fn revision_rejects_older_results() {
        let mut revision = Revision::default();
        let first = revision.advance();
        let second = revision.advance();

        assert!(!revision.is_current(first));
        assert!(revision.is_current(second));
    }
}
