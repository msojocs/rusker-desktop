#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EngineInfo {
    pub name: String,
    pub version: String,
    pub api_version: String,
    pub operating_system: String,
    pub architecture: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EngineSnapshot {
    pub engine: EngineInfo,
    pub containers: Vec<ContainerSummary>,
    pub images: Vec<ImageSummary>,
}

impl EngineSnapshot {
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
    pub state: String,
    pub status: String,
    pub created: i64,
    pub ports: Vec<PortMapping>,
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
    pub created: i64,
    pub size: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContainerAction {
    Start,
    Stop,
    Restart,
    Remove,
}

impl ContainerAction {
    pub fn label(self) -> &'static str {
        match self {
            Self::Start => "启动容器",
            Self::Stop => "停止容器",
            Self::Restart => "重启容器",
            Self::Remove => "删除容器",
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
