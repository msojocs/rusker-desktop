use std::collections::BTreeMap;

use bollard::models::CreateImageInfo;

/// Latest engine status and per-layer progress. Kept in a watch channel so
/// a fast download cannot build an unbounded queue of UI updates.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ImagePullProgress {
    pub status: String,
    pub layers: BTreeMap<String, ImagePullLayer>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ImagePullLayer {
    pub status: String,
    pub current: Option<u64>,
    pub total: Option<u64>,
}

impl ImagePullProgress {
    pub(crate) fn update(&mut self, response: CreateImageInfo) {
        let Some(status) = response.status.filter(|status| !status.is_empty()) else {
            return;
        };
        // Docker uses the tag as `id` for this repository-level event, not
        // a layer digest. Do not leave a fake layer in the progress list.
        if status.starts_with("Pulling from ") {
            self.status = status;
            return;
        }
        if let Some(id) = response.id.filter(|id| !id.is_empty()) {
            let layer = self.layers.entry(id).or_default();
            // Downloading and extracting use different byte counts. Never
            // carry a download percentage over to a subsequent phase.
            if layer.status != status {
                layer.current = None;
                layer.total = None;
            }
            layer.status = status;
            if let Some(detail) = response.progress_detail {
                layer.current = detail.current.and_then(|n| u64::try_from(n).ok());
                layer.total = detail.total.and_then(|n| u64::try_from(n).ok());
            }
        } else {
            self.status = status;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(json: &str) -> CreateImageInfo {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn tracks_layers_independently() {
        let mut progress = ImagePullProgress::default();
        progress.update(response(
            r#"{"status":"Pulling from library/alpine","id":"latest"}"#,
        ));
        progress.update(response(
            r#"{"status":"Downloading","id":"abc","progressDetail":{"current":50,"total":100}}"#,
        ));
        progress.update(response(r#"{"status":"Already exists","id":"def"}"#));
        assert_eq!(progress.layers.len(), 2);
        assert_eq!(progress.status, "Pulling from library/alpine");
        assert_eq!(progress.layers["abc"].current, Some(50));
        assert_eq!(progress.layers["abc"].total, Some(100));
        assert_eq!(progress.layers["def"].status, "Already exists");
        progress.update(response(
            r#"{"status":"Status: Downloaded newer image for alpine:latest"}"#,
        ));
        assert!(progress.status.contains("Downloaded newer image"));
        assert_eq!(progress.layers["abc"].current, Some(50));
    }

    #[test]
    fn clears_bytes_when_phase_changes() {
        let mut progress = ImagePullProgress::default();
        progress.update(response(
            r#"{"status":"Downloading","id":"abc","progressDetail":{"current":100,"total":100}}"#,
        ));
        progress.update(response(r#"{"status":"Extracting","id":"abc"}"#));
        assert_eq!(progress.layers["abc"].current, None);
        assert_eq!(progress.layers["abc"].total, None);
        progress.update(response(
            r#"{"status":"Extracting","id":"abc","progressDetail":{"current":20,"total":200}}"#,
        ));
        assert_eq!(progress.layers["abc"].current, Some(20));
        progress.update(response(r#"{"status":"Pull complete","id":"abc"}"#));
        assert_eq!(progress.layers["abc"].total, None);
    }

    #[test]
    fn ignores_empty_status_and_negative_counts() {
        let mut progress = ImagePullProgress::default();
        progress.update(response(r#"{"id":"abc","progressDetail":{}}"#));
        assert!(progress.layers.is_empty());
        progress.update(response(
            r#"{"status":"Downloading","id":"abc","progressDetail":{"current":-1,"total":-1}}"#,
        ));
        assert_eq!(progress.layers["abc"].current, None);
        assert_eq!(progress.layers["abc"].total, None);
    }
}
