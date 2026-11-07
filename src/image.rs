//! Image list logic for the Images page: one row per tag (like
//! `docker images`), usage by containers, filtering and sorting.

use std::{cmp::Ordering, collections::HashSet};

use crate::{
    format::short_id,
    model::{ContainerSummary, ImageSummary},
};

pub const NONE_LABEL: &str = "<none>";

/// One line of the image table. A multi-tagged image yields one row per tag.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageRow {
    /// Unique per row: the `repo:tag` reference, or the image ID if untagged.
    pub key: String,
    pub id: String,
    pub repository: String,
    pub tag: String,
    /// `repo:tag`; `None` for untagged (dangling) images.
    pub reference: Option<String>,
    pub created: i64,
    pub size: u64,
    /// Number of containers (running or not) created from this image.
    pub containers: usize,
}

impl ImageRow {
    pub fn in_use(&self) -> bool {
        self.containers > 0
    }

    pub fn dangling(&self) -> bool {
        self.reference.is_none()
    }

    /// What the engine accepts to run, pull or remove exactly this row.
    pub fn target(&self) -> &str {
        self.reference.as_deref().unwrap_or(&self.id)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ImageFilter {
    #[default]
    All,
    InUse,
    Unused,
    Dangling,
}

impl ImageFilter {
    pub const ALL: [Self; 4] = [Self::All, Self::InUse, Self::Unused, Self::Dangling];

    pub fn label(self) -> &'static str {
        match self {
            Self::All => "全部镜像",
            Self::InUse => "使用中",
            Self::Unused => "未使用",
            Self::Dangling => "悬空（无标签）",
        }
    }

    pub fn accepts(self, row: &ImageRow) -> bool {
        match self {
            Self::All => true,
            Self::InUse => row.in_use(),
            Self::Unused => !row.in_use(),
            Self::Dangling => row.dangling(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ImageSortKey {
    Name,
    Tag,
    Created,
    #[default]
    Size,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImageSort {
    pub key: ImageSortKey,
    pub descending: bool,
}

impl Default for ImageSort {
    fn default() -> Self {
        Self {
            key: ImageSortKey::Size,
            descending: true,
        }
    }
}

impl ImageSort {
    /// Clicking the active column flips the order; another column starts
    /// ascending for text and descending for dates and sizes.
    pub fn toggled(self, key: ImageSortKey) -> Self {
        if self.key == key {
            Self {
                key,
                descending: !self.descending,
            }
        } else {
            Self {
                key,
                descending: matches!(key, ImageSortKey::Created | ImageSortKey::Size),
            }
        }
    }

    pub fn apply(self, rows: &mut [ImageRow]) {
        rows.sort_by(|a, b| {
            let primary = match self.key {
                ImageSortKey::Name => compare_text(&a.repository, &b.repository),
                ImageSortKey::Tag => compare_text(&a.tag, &b.tag),
                ImageSortKey::Created => a.created.cmp(&b.created),
                ImageSortKey::Size => a.size.cmp(&b.size),
            };
            let primary = if self.descending {
                primary.reverse()
            } else {
                primary
            };
            primary
                .then_with(|| compare_text(&a.repository, &b.repository))
                .then_with(|| compare_text(&a.tag, &b.tag))
                .then_with(|| a.key.cmp(&b.key))
        });
    }
}

fn compare_text(a: &str, b: &str) -> Ordering {
    a.to_lowercase().cmp(&b.to_lowercase())
}

/// Splits `registry:5000/repo:tag` into repository and tag; the port of a
/// registry host is not mistaken for a tag.
pub fn split_reference(reference: &str) -> (String, String) {
    let name_start = reference.rfind('/').map_or(0, |slash| slash + 1);
    match reference[name_start..].rfind(':') {
        Some(colon) => {
            let colon = name_start + colon;
            (
                reference[..colon].to_string(),
                reference[colon + 1..].to_string(),
            )
        }
        None => (reference.to_string(), "latest".to_string()),
    }
}

fn is_real_tag(tag: &str) -> bool {
    !tag.is_empty() && tag != "<none>:<none>"
}

fn container_count(image: &ImageSummary, containers: &[ContainerSummary]) -> usize {
    containers
        .iter()
        .filter(|container| {
            if container.image_id.is_empty() {
                image.tags.iter().any(|tag| tag == &container.image)
                    || short_id(&image.id) == short_id(&container.image)
            } else {
                container.image_id == image.id
            }
        })
        .count()
}

pub fn image_rows(images: &[ImageSummary], containers: &[ContainerSummary]) -> Vec<ImageRow> {
    let mut rows = Vec::new();
    for image in images {
        let count = container_count(image, containers);
        let tags: Vec<&String> = image.tags.iter().filter(|tag| is_real_tag(tag)).collect();
        if tags.is_empty() {
            let repository = image
                .digests
                .iter()
                .find_map(|digest| digest.split_once('@').map(|(repo, _)| repo.to_string()))
                .filter(|repo| !repo.is_empty() && repo != NONE_LABEL)
                .unwrap_or_else(|| NONE_LABEL.into());
            rows.push(ImageRow {
                key: image.id.clone(),
                id: image.id.clone(),
                repository,
                tag: NONE_LABEL.into(),
                reference: None,
                created: image.created,
                size: image.size,
                containers: count,
            });
            continue;
        }
        for tag in tags {
            let (repository, tag_name) = split_reference(tag);
            rows.push(ImageRow {
                key: tag.clone(),
                id: image.id.clone(),
                repository,
                tag: tag_name,
                reference: Some(tag.clone()),
                created: image.created,
                size: image.size,
                containers: count,
            });
        }
    }
    rows
}

pub fn image_row_matches(row: &ImageRow, query: &str) -> bool {
    let haystack = format!(
        "{} {} {} {}",
        row.repository,
        row.tag,
        row.reference.as_deref().unwrap_or_default(),
        row.id.strip_prefix("sha256:").unwrap_or(&row.id)
    )
    .to_lowercase();
    query
        .split_whitespace()
        .all(|term| haystack.contains(&term.to_lowercase()))
}

/// `(in use, total)` bytes over distinct images.
pub fn image_disk_usage(rows: &[ImageRow]) -> (u64, u64) {
    let mut seen = HashSet::new();
    let (mut used, mut total) = (0, 0);
    for row in rows {
        if seen.insert(row.id.as_str()) {
            total += row.size;
            if row.in_use() {
                used += row.size;
            }
        }
    }
    (used, total)
}

/// Number of distinct images among the rows.
pub fn distinct_images(rows: &[ImageRow]) -> usize {
    rows.iter()
        .map(|row| row.id.as_str())
        .collect::<HashSet<_>>()
        .len()
}

/// Removal targets for every image no container uses: each tag (untagging
/// the last one deletes the image) or the ID of untagged images.
pub fn unused_image_targets(rows: &[ImageRow]) -> Vec<String> {
    rows.iter()
        .filter(|row| !row.in_use())
        .map(|row| row.target().to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(id: &str, tags: &[&str], size: u64, created: i64) -> ImageSummary {
        ImageSummary {
            id: id.into(),
            tags: tags.iter().map(|tag| tag.to_string()).collect(),
            digests: Vec::new(),
            created,
            size,
        }
    }

    fn container(image_id: &str) -> ContainerSummary {
        ContainerSummary {
            image_id: image_id.into(),
            ..ContainerSummary::default()
        }
    }

    #[test]
    fn splits_references_without_confusing_registry_ports() {
        assert_eq!(
            split_reference("nginx:alpine"),
            ("nginx".into(), "alpine".into())
        );
        assert_eq!(
            split_reference("localhost:5000/team/app:1.2"),
            ("localhost:5000/team/app".into(), "1.2".into())
        );
        assert_eq!(
            split_reference("localhost:5000/app"),
            ("localhost:5000/app".into(), "latest".into())
        );
    }

    #[test]
    fn one_row_per_tag_and_dangling_images_use_id() {
        let mut dangling = image("sha256:bbb", &["<none>:<none>"], 5, 2);
        dangling.digests = vec!["busybox@sha256:123".into()];
        let rows = image_rows(
            &[
                image("sha256:aaa", &["nginx:alpine", "nginx:stable"], 10, 1),
                dangling,
            ],
            &[container("sha256:aaa")],
        );
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].target(), "nginx:alpine");
        assert_eq!(rows[1].tag, "stable");
        assert!(rows[0].in_use() && rows[1].in_use());
        assert_eq!(rows[2].repository, "busybox");
        assert_eq!(rows[2].tag, NONE_LABEL);
        assert_eq!(rows[2].target(), "sha256:bbb");
        assert!(rows[2].dangling() && !rows[2].in_use());
    }

    #[test]
    fn disk_usage_counts_each_image_once() {
        let rows = image_rows(
            &[
                image("sha256:aaa", &["a:1", "a:2"], 10, 1),
                image("sha256:bbb", &["b:1"], 5, 1),
            ],
            &[container("sha256:aaa")],
        );
        assert_eq!(image_disk_usage(&rows), (10, 15));
        assert_eq!(distinct_images(&rows), 2);
        assert_eq!(unused_image_targets(&rows), vec!["b:1".to_string()]);
    }

    #[test]
    fn container_without_image_id_matches_by_reference() {
        let containers = [ContainerSummary {
            image: "a:1".into(),
            ..ContainerSummary::default()
        }];
        let rows = image_rows(&[image("sha256:aaa", &["a:1"], 1, 1)], &containers);
        assert!(rows[0].in_use());
    }

    #[test]
    fn sorting_and_toggling() {
        let mut rows = image_rows(
            &[
                image("sha256:aaa", &["b:1"], 10, 3),
                image("sha256:bbb", &["a:1"], 30, 1),
                image("sha256:ccc", &["c:1"], 20, 2),
            ],
            &[],
        );
        ImageSort::default().apply(&mut rows);
        let names: Vec<&str> = rows.iter().map(|row| row.repository.as_str()).collect();
        assert_eq!(names, ["a", "c", "b"]);

        let sort = ImageSort::default().toggled(ImageSortKey::Name);
        assert!(!sort.descending);
        sort.apply(&mut rows);
        let names: Vec<&str> = rows.iter().map(|row| row.repository.as_str()).collect();
        assert_eq!(names, ["a", "b", "c"]);
        assert!(sort.toggled(ImageSortKey::Name).descending);
    }

    #[test]
    fn filter_and_search() {
        let rows = image_rows(
            &[
                image("sha256:abc123", &["nginx:alpine"], 1, 1),
                image("sha256:def456", &[], 1, 1),
            ],
            &[container("sha256:abc123")],
        );
        assert!(ImageFilter::InUse.accepts(&rows[0]));
        assert!(ImageFilter::Dangling.accepts(&rows[1]));
        assert!(!ImageFilter::Unused.accepts(&rows[0]));
        assert!(image_row_matches(&rows[0], "NGINX alp"));
        assert!(image_row_matches(&rows[1], "def4"));
        assert!(!image_row_matches(&rows[0], "redis"));
    }
}
