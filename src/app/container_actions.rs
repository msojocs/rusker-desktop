//! Resource-scoped lifecycle actions; unrelated containers can run concurrently.

use super::*;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) enum ActionTarget {
    Container(String),
    ComposeProject(String),
}

#[derive(Default)]
pub(super) struct ContainerActions {
    pending: HashMap<ActionTarget, HashSet<String>>,
    tasks: HashMap<ActionTarget, Task<()>>,
}

impl ContainerActions {
    fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    fn container_busy(&self, id: &str) -> bool {
        self.pending.values().any(|ids| ids.contains(id))
    }

    fn project_busy(&self, project: &str, containers: &[ContainerSummary]) -> bool {
        self.pending
            .contains_key(&ActionTarget::ComposeProject(project.to_string()))
            || containers.iter().any(|container| {
                container.compose_project.as_deref() == Some(project)
                    && self.container_busy(&container.id)
            })
    }
}

impl RuskerApp {
    pub(super) fn container_action_busy(&self, id: &str) -> bool {
        (self.busy.is_some() && self.container_actions.is_empty())
            || self.container_actions.container_busy(id)
    }

    pub(super) fn compose_project_action_busy(&self, project: &str) -> bool {
        (self.busy.is_some() && self.container_actions.is_empty())
            || self
                .container_actions
                .project_busy(project, &self.snapshot.containers)
    }

    pub(super) fn run_container_action(
        &mut self,
        target: ActionTarget,
        ids: HashSet<String>,
        action: ContainerAction,
        handle: JoinHandle<Result<(EngineSnapshot, Vec<String>), EngineError>>,
        cx: &mut Context<Self>,
    ) {
        // Invalidate any earlier refresh once per concurrent batch. Other global
        // operations remain blocked until every lifecycle task has completed.
        if self.container_actions.is_empty() {
            self.refresh_revision.advance();
        }
        let revision = self.refresh_revision.current();
        self.container_actions.pending.insert(target.clone(), ids);
        self.busy = Some(t!("app.operation_progress", operation = action.label()).into());
        self.loading = false;
        cx.notify();
        let task_target = target.clone();
        let task = cx.spawn(async move |this, cx| {
            let result = await_engine(handle).await;
            _ = this.update(cx, |this, cx| {
                let ids = this
                    .container_actions
                    .pending
                    .remove(&task_target)
                    .unwrap_or_default();
                this.container_actions.tasks.remove(&task_target);
                let finished = this.container_actions.is_empty();
                if finished {
                    this.busy = None;
                }
                if this.refresh_revision.is_current(revision) {
                    match result {
                        Ok((snapshot, failures)) => {
                            this.connected = true;
                            // A concurrent task's snapshot may predate another
                            // task's completion. Only update our own resources.
                            let snapshot = merge_action_snapshot(&this.snapshot, &snapshot, &ids);
                            this.apply_snapshot(snapshot, cx);
                            this.after_volume_snapshot(cx);
                            this.sync_container_tools(cx);
                            if failures.is_empty() {
                                this.notify_success(
                                    t!("app.operation_done", operation = action.label()),
                                    cx,
                                );
                            } else {
                                this.notify_error(failures.join("\n"), cx);
                            }
                        }
                        Err(error) => this.notify_error(error.to_string(), cx),
                    }
                    // Reconcile the entire snapshot after all mutations, even
                    // if a task failed after partially changing the engine.
                    if finished {
                        this.refresh(cx);
                    }
                }
                cx.notify();
            });
        });
        self.container_actions.tasks.insert(target, task);
    }
}

fn merge_action_snapshot(
    current: &EngineSnapshot,
    incoming: &EngineSnapshot,
    ids: &HashSet<String>,
) -> EngineSnapshot {
    let mut snapshot = current.clone();
    snapshot.containers = current
        .containers
        .iter()
        .filter_map(|container| {
            if ids.contains(&container.id) {
                incoming
                    .containers
                    .iter()
                    .find(|item| item.id == container.id)
            } else {
                Some(container)
            }
        })
        .cloned()
        .collect();
    snapshot
}

#[cfg(test)]
mod tests {
    use super::*;

    fn container(id: &str, project: Option<&str>) -> ContainerSummary {
        ContainerSummary {
            id: id.into(),
            compose_project: project.map(String::from),
            ..ContainerSummary::default()
        }
    }

    #[test]
    fn individual_actions_only_lock_their_own_container() {
        let mut actions = ContainerActions::default();
        actions.pending.insert(
            ActionTarget::Container("a".into()),
            HashSet::from(["a".into()]),
        );
        assert!(actions.container_busy("a"));
        assert!(!actions.container_busy("b"));
        // Membership in the same Compose project does not lock a sibling.
        let containers = vec![container("a", Some("shop")), container("b", Some("shop"))];
        assert!(actions.project_busy("shop", &containers));
        assert!(!actions.project_busy("other", &containers));
        actions.pending.insert(
            ActionTarget::Container("b".into()),
            HashSet::from(["b".into()]),
        );
        actions.pending.remove(&ActionTarget::Container("a".into()));
        assert!(!actions.container_busy("a"));
        assert!(actions.container_busy("b"));
        assert!(!actions.is_empty());
    }

    #[test]
    fn compose_actions_lock_all_members_but_not_other_projects() {
        let mut actions = ContainerActions::default();
        let shop = ActionTarget::ComposeProject("shop".into());
        actions
            .pending
            .insert(shop.clone(), HashSet::from(["web".into(), "db".into()]));
        let containers = vec![
            container("web", Some("shop")),
            container("db", Some("shop")),
            container("worker", Some("jobs")),
            container("standalone", None),
        ];
        assert!(actions.project_busy("shop", &containers));
        assert!(actions.container_busy("web"));
        assert!(actions.container_busy("db"));
        assert!(!actions.container_busy("standalone"));
        assert!(!actions.container_busy("worker"));
        assert!(!actions.project_busy("jobs", &containers));
        actions.pending.insert(
            ActionTarget::ComposeProject("jobs".into()),
            HashSet::from(["worker".into()]),
        );
        actions.pending.remove(&shop);
        assert!(!actions.project_busy("shop", &containers));
        assert!(actions.project_busy("jobs", &containers));
        assert!(!actions.is_empty());
        actions.pending.clear();
        assert!(actions.is_empty());
        assert!(!actions.container_busy("worker"));
    }

    #[test]
    fn concurrent_snapshots_do_not_revert_other_containers() {
        let mut current = EngineSnapshot {
            containers: vec![container("a", None), container("b", None)],
            ..EngineSnapshot::default()
        };
        current.containers[0].state = "running".into();
        let mut incoming = current.clone();
        incoming.containers[0].state = "exited".into();
        incoming.containers[1].state = "running".into();
        let merged = merge_action_snapshot(&current, &incoming, &HashSet::from(["b".into()]));
        assert!(merged.containers.iter().all(ContainerSummary::is_running));
    }
}
