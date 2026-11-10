//! Engine-independent helpers for volume management: name and path
//! validation, the shell snippets run by the helper container, and parsing
//! of their output.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use time::{format_description::well_known::Rfc3339, OffsetDateTime};

use crate::model::{ContainerSummary, VolumeEntry, VolumeEntryKind, VolumeSummary};

/// Small image used for helper containers that read or modify volume data.
/// It is pulled on first use when missing.
pub const HELPER_IMAGE: &str = "busybox:1.37";
/// Label put on every helper container so leftovers are recognizable.
pub const HELPER_LABEL: &str = "dev.rusker.helper";
/// Mount point of the target volume inside helper containers.
pub const VOLUME_MOUNT: &str = "/volume";
/// Maximum number of entries returned for a single directory.
pub const MAX_DIRECTORY_ENTRIES: usize = 5000;

const DU_MARKER: &str = "--rusker-du--";

/// Lists one directory: `stat` per entry, then per-subdirectory disk usage.
/// The directory is passed in `RUSKER_PATH` so it is never shell-parsed.
pub const LIST_SCRIPT: &str = r#"cd "$RUSKER_PATH" || exit 3
find . -mindepth 1 -maxdepth 1 -exec stat -c '%F|%s|%Y|%A|%n' {} + 2>/dev/null
echo '--rusker-du--'
du -k -d 1 . 2>/dev/null
exit 0"#;

/// Removes everything inside the volume, including dotfiles, but keeps the
/// mount point itself.
pub const EMPTY_SCRIPT: &str = "rm -rf /volume/..?* /volume/.[!.]* /volume/*";

/// Copies a read-only `/from` volume into `/to`, preserving ownership,
/// permissions, timestamps and links.
pub const CLONE_SCRIPT: &str = "cp -a /from/. /to/";

/// Archive source for exports: the trailing `/.` makes the engine emit
/// entries relative to the volume root (`./file`), so an export can be
/// imported again unchanged.
pub const EXPORT_SOURCE: &str = "/volume/.";

/// Default file name for an export, e.g. `db-data-20251106-083000.tar`.
pub fn export_file_name(volume: &str, at: OffsetDateTime) -> String {
    let format = time::macros::format_description!("[year][month][day]-[hour][minute][second]");
    let stamp = at.format(&format).unwrap_or_default();
    format!("{volume}-{stamp}.tar")
}

/// Temporary sibling an export is written to before it is renamed.
pub fn partial_export_path(destination: &Path) -> PathBuf {
    let mut name = destination
        .file_name()
        .map(|name| name.to_os_string())
        .unwrap_or_else(|| "export.tar".into());
    name.push(".partial");
    destination.with_file_name(name)
}

/// Unused volumes (not mounted by any container) among `volumes`.
pub fn unused_volumes<'a>(
    volumes: &'a [VolumeSummary],
    containers: &[ContainerSummary],
) -> Vec<&'a VolumeSummary> {
    volumes
        .iter()
        .filter(|volume| {
            !containers
                .iter()
                .any(|container| container.volumes.iter().any(|name| name == &volume.name))
        })
        .collect()
}

pub fn validate_volume_name(name: &str) -> Result<(), String> {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return Err(t!("volume.name_empty").into_owned());
    };
    if !first.is_ascii_alphanumeric()
        || !chars.all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '_' | '.' | '-')
        })
    {
        return Err(t!("volume.name_invalid").into_owned());
    }
    if name.len() < 2 {
        return Err(t!("volume.name_too_short").into_owned());
    }
    Ok(())
}

/// Turns a path relative to the volume root (`""`, `"a"`, `"a/b"`) into the
/// absolute path inside the helper container, rejecting traversal.
pub fn container_path(relative: &str) -> Result<String, String> {
    let mut path = String::from(VOLUME_MOUNT);
    for segment in relative.split('/').filter(|segment| !segment.is_empty()) {
        if segment == "." || segment == ".." || segment.contains('\0') {
            return Err(t!("volume.invalid_path", relative = relative).into_owned());
        }
        path.push('/');
        path.push_str(segment);
    }
    Ok(path)
}

/// Joins a directory path relative to the volume root with a child name.
pub fn join_relative(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_string()
    } else {
        format!("{parent}/{name}")
    }
}

/// Output of [`LIST_SCRIPT`] turned into entries, sorted by name.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DirectoryListing {
    pub entries: Vec<VolumeEntry>,
    pub truncated: bool,
}

pub fn parse_listing(output: &str) -> DirectoryListing {
    let (stat_part, du_part) = match output.split_once(DU_MARKER) {
        Some((stat, du)) => (stat, du),
        None => (output, ""),
    };

    let mut directory_sizes: HashMap<&str, u64> = HashMap::new();
    for line in du_part.lines() {
        let Some((size, name)) = line.split_once('\t') else {
            continue;
        };
        let Some(name) = name.strip_prefix("./") else {
            continue;
        };
        if let Ok(kib) = size.trim().parse::<u64>() {
            directory_sizes.insert(name, kib.saturating_mul(1024));
        }
    }

    let mut entries = Vec::new();
    let mut truncated = false;
    for line in stat_part.lines() {
        let mut fields = line.splitn(5, '|');
        let (Some(kind), Some(size), Some(modified), Some(mode), Some(name)) = (
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
        ) else {
            continue;
        };
        let Some(name) = name.strip_prefix("./") else {
            continue;
        };
        if name.is_empty() || name.contains('/') {
            continue;
        }
        if entries.len() >= MAX_DIRECTORY_ENTRIES {
            truncated = true;
            break;
        }
        let kind = match kind {
            "directory" => VolumeEntryKind::Directory,
            "regular file" | "regular empty file" => VolumeEntryKind::File,
            "symbolic link" => VolumeEntryKind::Symlink,
            _ => VolumeEntryKind::Other,
        };
        let size = match kind {
            VolumeEntryKind::Directory => directory_sizes.get(name).copied().unwrap_or(0),
            _ => size.trim().parse().unwrap_or(0),
        };
        entries.push(VolumeEntry {
            name: name.to_string(),
            kind,
            size,
            modified: modified.trim().parse().unwrap_or(0),
            mode: mode.to_string(),
        });
    }
    entries.sort_by(|left, right| {
        left.name
            .to_lowercase()
            .cmp(&right.name.to_lowercase())
            .then_with(|| left.name.cmp(&right.name))
    });
    DirectoryListing { entries, truncated }
}

pub fn parse_timestamp(value: &str) -> i64 {
    OffsetDateTime::parse(value, &Rfc3339)
        .map(OffsetDateTime::unix_timestamp)
        .unwrap_or(0)
}

pub fn volume_matches(volume: &VolumeSummary, query: &str) -> bool {
    let haystack = format!("{} {}", volume.name, volume.driver).to_lowercase();
    query
        .split_whitespace()
        .all(|term| haystack.contains(&term.to_lowercase()))
}

/// Suggested name for a clone of `source` that does not collide with
/// `existing` names.
pub fn clone_name(source: &str, existing: &[VolumeSummary]) -> String {
    let taken = |name: &str| existing.iter().any(|volume| volume.name == name);
    let base = format!("{source}-clone");
    if !taken(&base) {
        return base;
    }
    (2..)
        .map(|index| format!("{base}-{index}"))
        .find(|name| !taken(name))
        .expect("unbounded range always yields a free name")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_volume_names_like_the_engine() {
        assert!(validate_volume_name("db-data").is_ok());
        assert!(validate_volume_name("a1.b_c-d").is_ok());
        assert!(validate_volume_name("").is_err());
        assert!(validate_volume_name("a").is_err());
        assert!(validate_volume_name("-data").is_err());
        assert!(validate_volume_name("my data").is_err());
        assert!(validate_volume_name("数据").is_err());
    }

    #[test]
    fn container_paths_reject_traversal() {
        assert_eq!(container_path("").unwrap(), "/volume");
        assert_eq!(container_path("a/b").unwrap(), "/volume/a/b");
        assert_eq!(container_path("/a//b/").unwrap(), "/volume/a/b");
        assert!(container_path("../etc").is_err());
        assert!(container_path("a/./b").is_err());
        assert_eq!(join_relative("", "x"), "x");
        assert_eq!(join_relative("a", "x"), "a/x");
    }

    #[test]
    fn parses_listing_with_directory_sizes() {
        let output = "regular file|4194304|1700000000|-rw-r-----|./#ib_16384_0.dblwr\n\
directory|4096|1700000001|drwxr-x---|./#innodb_redo\n\
regular empty file|0|1700000002|-rw-r--r--|./empty\n\
symbolic link|7|1700000003|lrwxrwxrwx|./link|with|pipes\n\
garbage line\n\
--rusker-du--\n\
102400\t./#innodb_redo\n\
106500\t.\n";
        let listing = parse_listing(output);
        assert!(!listing.truncated);
        let names: Vec<&str> = listing.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "#ib_16384_0.dblwr",
                "#innodb_redo",
                "empty",
                "link|with|pipes"
            ]
        );
        let redo = &listing.entries[1];
        assert_eq!(redo.kind, VolumeEntryKind::Directory);
        assert_eq!(redo.size, 102400 * 1024);
        assert_eq!(redo.mode, "drwxr-x---");
        assert_eq!(listing.entries[0].size, 4194304);
        assert_eq!(listing.entries[0].modified, 1700000000);
        assert_eq!(listing.entries[2].kind, VolumeEntryKind::File);
        assert_eq!(listing.entries[3].kind, VolumeEntryKind::Symlink);
    }

    #[test]
    fn empty_listing_is_empty() {
        assert_eq!(
            parse_listing("--rusker-du--\n4\t.\n"),
            DirectoryListing::default()
        );
    }

    #[test]
    fn parses_engine_timestamps() {
        assert_eq!(parse_timestamp("2023-11-14T22:13:20Z"), 1700000000);
        assert_eq!(parse_timestamp("2023-11-15T06:13:20+08:00"), 1700000000);
        assert_eq!(parse_timestamp(""), 0);
    }

    #[test]
    fn export_names_and_partial_paths() {
        let at = OffsetDateTime::from_unix_timestamp(1700000000).unwrap();
        assert_eq!(export_file_name("db", at), "db-20231114-221320.tar");
        assert_eq!(
            partial_export_path(Path::new("out/db.tar")),
            Path::new("out/db.tar.partial")
        );
    }

    #[test]
    fn finds_unused_volumes() {
        let volume = |name: &str| VolumeSummary {
            name: name.into(),
            ..VolumeSummary::default()
        };
        let volumes = [volume("a"), volume("b"), volume("c")];
        let containers = [ContainerSummary {
            volumes: vec!["b".into()],
            ..ContainerSummary::default()
        }];
        let unused: Vec<&str> = unused_volumes(&volumes, &containers)
            .into_iter()
            .map(|volume| volume.name.as_str())
            .collect();
        assert_eq!(unused, ["a", "c"]);
    }

    #[test]
    fn suggests_free_clone_names() {
        let volume = |name: &str| VolumeSummary {
            name: name.into(),
            ..VolumeSummary::default()
        };
        assert_eq!(clone_name("data", &[volume("data")]), "data-clone");
        assert_eq!(
            clone_name("data", &[volume("data-clone"), volume("data-clone-2")]),
            "data-clone-3"
        );
    }
}
