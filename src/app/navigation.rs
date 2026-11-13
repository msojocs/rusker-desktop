//! Navigation within the container section. The list is the root page.

use rusker_desktop::model::ContainerSummary;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) enum ContainerPage {
    #[default]
    List,
    ComposeProject(String),
    ComposeConfig(String),
    Container(String),
}

impl ContainerPage {
    fn exists(&self, containers: &[ContainerSummary]) -> bool {
        match self {
            Self::List => true,
            Self::ComposeProject(project) | Self::ComposeConfig(project) => containers
                .iter()
                .any(|container| container.compose_project.as_ref() == Some(project)),
            Self::Container(id) => containers.iter().any(|container| &container.id == id),
        }
    }
}

#[derive(Default)]
pub(super) struct ContainerNavigation {
    current: ContainerPage,
    history: Vec<ContainerPage>,
}

impl ContainerNavigation {
    pub(super) fn current(&self) -> &ContainerPage {
        &self.current
    }

    pub(super) fn container(&self) -> Option<&String> {
        match &self.current {
            ContainerPage::Container(id) => Some(id),
            _ => None,
        }
    }

    pub(super) fn compose_project(&self) -> Option<&String> {
        match &self.current {
            ContainerPage::ComposeProject(project) | ContainerPage::ComposeConfig(project) => {
                Some(project)
            }
            _ => None,
        }
    }

    pub(super) fn parent_project(&self) -> Option<&String> {
        match self.history.last() {
            Some(ContainerPage::ComposeProject(project)) => Some(project),
            _ => None,
        }
    }

    pub(super) fn push(&mut self, page: ContainerPage) {
        if self.current != page {
            self.history
                .push(std::mem::replace(&mut self.current, page));
        }
    }

    pub(super) fn back(&mut self) {
        self.current = self.history.pop().unwrap_or_default();
    }

    pub(super) fn reset(&mut self) {
        *self = Self::default();
    }

    /// Drop deleted destinations and return to the nearest surviving page.
    /// Returns whether the visible page changed and needs its data reloaded.
    pub(super) fn reconcile(&mut self, containers: &[ContainerSummary]) -> bool {
        self.history.retain(|page| page.exists(containers));
        if self.current.exists(containers) {
            return false;
        }
        self.back();
        true
    }
}

#[cfg(test)]
mod tests {
    use super::{ContainerNavigation, ContainerPage};
    use rusker_desktop::model::ContainerSummary;

    fn service(id: &str) -> ContainerSummary {
        ContainerSummary {
            id: id.into(),
            compose_project: Some("demo".into()),
            ..Default::default()
        }
    }

    #[test]
    fn compose_service_returns_through_project_to_list() {
        let mut navigation = ContainerNavigation::default();
        navigation.push(ContainerPage::ComposeProject("demo".into()));
        navigation.push(ContainerPage::Container("web".into()));
        assert_eq!(navigation.container().map(String::as_str), Some("web"));
        assert_eq!(
            navigation.parent_project().map(String::as_str),
            Some("demo")
        );
        navigation.back();
        assert_eq!(
            navigation.compose_project().map(String::as_str),
            Some("demo")
        );
        navigation.back();
        assert_eq!(navigation.current(), &ContainerPage::List);
        navigation.back();
        assert_eq!(navigation.current(), &ContainerPage::List);
    }

    #[test]
    fn service_opened_from_list_returns_to_list() {
        let mut navigation = ContainerNavigation::default();
        navigation.push(ContainerPage::Container("web".into()));
        assert!(navigation.parent_project().is_none());
        navigation.back();
        assert_eq!(navigation.current(), &ContainerPage::List);
    }

    #[test]
    fn repeated_selection_does_not_add_a_back_step() {
        let mut navigation = ContainerNavigation::default();
        navigation.push(ContainerPage::ComposeProject("demo".into()));
        navigation.push(ContainerPage::Container("web".into()));
        navigation.push(ContainerPage::Container("web".into()));
        navigation.back();
        assert_eq!(
            navigation.compose_project().map(String::as_str),
            Some("demo")
        );
    }

    #[test]
    fn config_returns_to_project_and_reset_discards_history() {
        let mut navigation = ContainerNavigation::default();
        navigation.push(ContainerPage::ComposeProject("demo".into()));
        navigation.push(ContainerPage::ComposeConfig("demo".into()));
        navigation.back();
        assert_eq!(
            navigation.current(),
            &ContainerPage::ComposeProject("demo".into())
        );
        navigation.push(ContainerPage::Container("web".into()));
        navigation.reset();
        navigation.push(ContainerPage::Container("db".into()));
        navigation.back();
        assert_eq!(navigation.current(), &ContainerPage::List);
    }

    #[test]
    fn deleted_service_returns_to_surviving_project() {
        let mut navigation = ContainerNavigation::default();
        navigation.push(ContainerPage::ComposeProject("demo".into()));
        navigation.push(ContainerPage::Container("web".into()));
        assert!(navigation.reconcile(&[service("db")]));
        assert_eq!(
            navigation.compose_project().map(String::as_str),
            Some("demo")
        );
        assert!(navigation.reconcile(&[]));
        assert_eq!(navigation.current(), &ContainerPage::List);
    }

    #[test]
    fn deleted_parent_is_skipped_when_returning() {
        let mut navigation = ContainerNavigation::default();
        navigation.push(ContainerPage::ComposeProject("demo".into()));
        navigation.push(ContainerPage::Container("other".into()));
        assert!(!navigation.reconcile(&[ContainerSummary {
            id: "other".into(),
            ..Default::default()
        }]));
        assert!(navigation.parent_project().is_none());
        navigation.back();
        assert_eq!(navigation.current(), &ContainerPage::List);
    }
}
