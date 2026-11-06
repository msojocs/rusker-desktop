//! Docker data disk, the counterpart of Docker Desktop's `docker_data.vhdx`.
//!
//! The `rusker` distribution's own `ext4.vhdx` only holds the OS. Images,
//! containers and volumes live on a separate VHDX that Rusker creates and
//! attaches to the WSL2 utility VM with `wsl --mount --vhd --bare`. WSL keeps
//! a registered distribution's disk attached until the whole VM shuts down,
//! but a bare-mounted disk can be detached on its own with `wsl --unmount`,
//! so the data disk can be moved without stopping other distributions.
//!
//! Inside the VM the disk is found by its SCSI WWID, which Hyper-V derives
//! from the VHDX's unique ID: `naa.60022480` + bytes 0..6 + bytes 10..16.

use std::{
    fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::RuntimeHostError;

pub const DATA_DISK_FILE: &str = "docker-data.vhdx";
const STATE_FILE: &str = "data-disk.json";
/// Maximum (virtual) size of the dynamically expanding disk.
const DATA_DISK_MAX_SIZE: u64 = 1 << 40;

/// Runs inside the managed distribution as root. `$1` is the WWID to look
/// for. Formats the disk on first use (only when it carries no signature at
/// all), migrates data an older install kept on the system disk, then bind
/// mounts `/var/lib/docker` and `/var/lib/containerd` from it. Exit code 3
/// means the disk is not attached.
pub const MOUNT_SCRIPT: &str = r#"set -eu
want="naa.$1"
DATA=/mnt/rusker-data
dev=""
i=0
while [ -z "$dev" ] && [ "$i" -lt 30 ]; do
    for block in /sys/block/sd*; do
        if [ "$(cat "$block/device/wwid" 2>/dev/null || true)" = "$want" ]; then
            dev="/dev/${block##*/}"
            break
        fi
    done
    if [ -n "$dev" ]; then
        break
    fi
    sleep 0.5
    i=$((i + 1))
done
if [ -z "$dev" ]; then
    echo "未找到 Docker 数据磁盘（$want）" >&2
    exit 3
fi
if mountpoint -q "$DATA"; then
    current=$(findmnt -n -o SOURCE "$DATA")
    if [ "$current" != "$dev" ]; then
        echo "$DATA 已挂载其他设备 $current" >&2
        exit 4
    fi
else
    rc=0
    blkid -p "$dev" >/dev/null 2>&1 || rc=$?
    if [ "$rc" -eq 2 ]; then
        mkfs.ext4 -q -E nodiscard,lazy_itable_init=1,lazy_journal_init=1 -L rusker-data "$dev"
    elif [ "$rc" -ne 0 ]; then
        echo "无法识别 Docker 数据磁盘 $dev 的文件系统（blkid 退出码 $rc）" >&2
        exit 5
    fi
    mkdir -p "$DATA"
    mount "$dev" "$DATA"
fi
for name in docker containerd; do
    target="/var/lib/$name"
    if mountpoint -q "$target"; then
        continue
    fi
    mkdir -p "$target" "$DATA/$name"
    if [ ! -e "$DATA/.migrated-$name" ]; then
        pid=$(cat /var/run/docker.pid 2>/dev/null || true)
        if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
            kill -TERM "$pid"
            j=0
            while kill -0 "$pid" 2>/dev/null && [ "$j" -lt 40 ]; do
                sleep 1
                j=$((j + 1))
            done
        fi
        if [ -n "$(ls -A "$target")" ]; then
            cp -a "$target/." "$DATA/$name/"
            sync
        fi
        : > "$DATA/.migrated-$name"
        sync
        find "$target" -mindepth 1 -maxdepth 1 -exec rm -rf {} +
    fi
    mount --bind "$DATA/$name" "$target"
done
"#;

/// Where the data disk lives and how to recognise it inside the VM.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DataDisk {
    pub path: PathBuf,
    /// The VHDX unique ID as 32 hex digits, in on-disk byte order.
    pub id: String,
}

impl DataDisk {
    pub fn directory(&self) -> PathBuf {
        self.path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| self.path.clone())
    }

    /// SCSI WWID (without the `naa.` prefix) the VM reports for this disk.
    pub fn wwid(&self) -> Result<String, RuntimeHostError> {
        let id = &self.id;
        if id.len() != 32 || !id.chars().all(|character| character.is_ascii_hexdigit()) {
            return Err(RuntimeHostError::State(format!(
                "Docker 数据磁盘 ID 无效：{id}"
            )));
        }
        Ok(format!("60022480{}{}", &id[..12], &id[20..]).to_lowercase())
    }
}

pub fn state_path(root: &Path) -> PathBuf {
    root.join(STATE_FILE)
}

pub fn load(root: &Path) -> Result<Option<DataDisk>, RuntimeHostError> {
    match fs::read_to_string(state_path(root)) {
        Ok(text) => serde_json::from_str(&text)
            .map(Some)
            .map_err(|error| RuntimeHostError::State(format!("{STATE_FILE}：{error}"))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(RuntimeHostError::Io {
            action: "读取 Docker 数据磁盘记录",
            source,
        }),
    }
}

/// Atomically replaces the state file.
pub fn save(root: &Path, disk: &DataDisk) -> Result<(), RuntimeHostError> {
    let io = |source| RuntimeHostError::Io {
        action: "写入 Docker 数据磁盘记录",
        source,
    };
    fs::create_dir_all(root).map_err(io)?;
    let text = serde_json::to_string_pretty(disk)
        .map_err(|error| RuntimeHostError::State(error.to_string()))?;
    let temporary = root.join(format!("{STATE_FILE}.tmp"));
    fs::write(&temporary, text).map_err(io)?;
    fs::rename(&temporary, state_path(root)).map_err(io)
}

/// Creates a new, empty dynamically expanding VHDX in `directory`.
pub fn create(directory: &Path) -> Result<DataDisk, RuntimeHostError> {
    fs::create_dir_all(directory).map_err(|source| RuntimeHostError::Io {
        action: "创建 Docker 数据磁盘目录",
        source,
    })?;
    let path = directory.join(DATA_DISK_FILE);
    if path.exists() {
        return Err(RuntimeHostError::Conflict(format!(
            "{} 已存在，拒绝覆盖",
            path.display()
        )));
    }
    let id = random_id(&path);
    create_vhdx(&path, &id, DATA_DISK_MAX_SIZE)?;
    Ok(DataDisk { path, id: hex(&id) })
}

/// Moves the (detached) disk file into `directory`. A rename is used on the
/// same volume; otherwise the file is copied and the original removed only
/// after the copy succeeded.
pub fn move_file(disk: &DataDisk, directory: &Path) -> Result<DataDisk, RuntimeHostError> {
    let io = |action, source| RuntimeHostError::Io { action, source };
    fs::create_dir_all(directory).map_err(|source| io("创建目标目录", source))?;
    let target = directory.join(DATA_DISK_FILE);
    if target.exists() {
        return Err(RuntimeHostError::Conflict(format!(
            "目标目录已有 {}，拒绝覆盖",
            target.display()
        )));
    }
    if fs::rename(&disk.path, &target).is_err() {
        let partial = directory.join(format!("{DATA_DISK_FILE}.partial"));
        if let Err(source) = fs::copy(&disk.path, &partial) {
            let _ = fs::remove_file(&partial);
            return Err(io("复制 Docker 数据磁盘", source));
        }
        if let Err(source) = fs::rename(&partial, &target) {
            let _ = fs::remove_file(&partial);
            return Err(io("保存复制的 Docker 数据磁盘", source));
        }
        if let Err(source) = fs::remove_file(&disk.path) {
            let _ = fs::remove_file(&target);
            return Err(io("删除原 Docker 数据磁盘", source));
        }
    }
    Ok(DataDisk {
        path: target,
        id: disk.id.clone(),
    })
}

fn random_id(path: &Path) -> [u8; 16] {
    let mut hasher = Sha256::new();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    hasher.update(now.as_nanos().to_le_bytes());
    hasher.update(std::process::id().to_le_bytes());
    hasher.update(path.to_string_lossy().as_bytes());
    let local = 0_u8;
    hasher.update((&local as *const u8 as usize).to_le_bytes());
    let digest = hasher.finalize();
    let mut id = [0_u8; 16];
    id.copy_from_slice(&digest[..16]);
    // RFC 4122 version 4 / variant bits, so the ID is a well-formed GUID.
    id[7] = (id[7] & 0x0f) | 0x40;
    id[8] = (id[8] & 0x3f) | 0x80;
    id
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(windows)]
fn create_vhdx(path: &Path, id: &[u8; 16], size: u64) -> Result<(), RuntimeHostError> {
    use std::{ffi::c_void, os::windows::ffi::OsStrExt};

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Guid {
        data1: u32,
        data2: u16,
        data3: u16,
        data4: [u8; 8],
    }

    #[repr(C)]
    struct VirtualStorageType {
        device_id: u32,
        vendor_id: Guid,
    }

    /// `CREATE_VIRTUAL_DISK_PARAMETERS::Version2`. Nested so the union
    /// starts at offset 8 like in the C header.
    #[repr(C)]
    struct Version2 {
        unique_id: Guid,
        maximum_size: u64,
        block_size_in_bytes: u32,
        sector_size_in_bytes: u32,
        physical_sector_size_in_bytes: u32,
        parent_path: *const u16,
        source_path: *const u16,
        open_flags: u32,
        parent_virtual_storage_type: VirtualStorageType,
        source_virtual_storage_type: VirtualStorageType,
        resiliency_guid: Guid,
    }

    #[repr(C)]
    struct CreateParameters {
        version: u32,
        version2: Version2,
    }

    #[link(name = "virtdisk")]
    extern "system" {
        fn CreateVirtualDisk(
            virtual_storage_type: *const VirtualStorageType,
            path: *const u16,
            access_mask: u32,
            security_descriptor: *const c_void,
            flags: u32,
            provider_specific_flags: u32,
            parameters: *const CreateParameters,
            overlapped: *const c_void,
            handle: *mut *mut c_void,
        ) -> u32;
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn CloseHandle(handle: *mut c_void) -> i32;
    }

    const VIRTUAL_STORAGE_TYPE_DEVICE_VHDX: u32 = 3;
    const VENDOR_MICROSOFT: Guid = Guid {
        data1: 0xEC98_4AEC,
        data2: 0xA0F9,
        data3: 0x47E9,
        data4: [0x90, 0x1F, 0x71, 0x41, 0x5A, 0x66, 0x34, 0x5B],
    };
    const CREATE_VIRTUAL_DISK_VERSION_2: u32 = 2;
    let zero = Guid {
        data1: 0,
        data2: 0,
        data3: 0,
        data4: [0; 8],
    };
    let unique_id = Guid {
        data1: u32::from_le_bytes([id[0], id[1], id[2], id[3]]),
        data2: u16::from_le_bytes([id[4], id[5]]),
        data3: u16::from_le_bytes([id[6], id[7]]),
        data4: [id[8], id[9], id[10], id[11], id[12], id[13], id[14], id[15]],
    };
    let storage_type = VirtualStorageType {
        device_id: VIRTUAL_STORAGE_TYPE_DEVICE_VHDX,
        vendor_id: VENDOR_MICROSOFT,
    };
    let parameters = CreateParameters {
        version: CREATE_VIRTUAL_DISK_VERSION_2,
        version2: Version2 {
            unique_id,
            maximum_size: size,
            // 1 MiB like WSL's own disks; the 32 MiB default makes mkfs
            // metadata spread over the disk balloon an empty VHDX to ~17 GiB.
            block_size_in_bytes: 1 << 20,
            sector_size_in_bytes: 0,
            physical_sector_size_in_bytes: 0,
            parent_path: std::ptr::null(),
            source_path: std::ptr::null(),
            open_flags: 0,
            parent_virtual_storage_type: VirtualStorageType {
                device_id: 0,
                vendor_id: zero,
            },
            source_virtual_storage_type: VirtualStorageType {
                device_id: 0,
                vendor_id: zero,
            },
            resiliency_guid: zero,
        },
    };
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut handle = std::ptr::null_mut();
    // SAFETY: every pointer refers to a live, correctly laid out value for
    // the duration of the call; the returned handle is closed below.
    let status = unsafe {
        CreateVirtualDisk(
            &storage_type,
            wide.as_ptr(),
            0,
            std::ptr::null(),
            0,
            0,
            &parameters,
            std::ptr::null(),
            &mut handle,
        )
    };
    if status != 0 {
        return Err(RuntimeHostError::Io {
            action: "创建 Docker 数据磁盘",
            source: std::io::Error::from_raw_os_error(status as i32),
        });
    }
    // SAFETY: `handle` was returned by a successful CreateVirtualDisk.
    unsafe {
        CloseHandle(handle);
    }
    Ok(())
}

#[cfg(not(windows))]
fn create_vhdx(_path: &Path, _id: &[u8; 16], _size: u64) -> Result<(), RuntimeHostError> {
    Err(RuntimeHostError::Unsupported)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_hyper_v_wwid_from_unique_id() {
        // Observed in a WSL2 VM: a VHDX whose unique ID bytes are
        // 66 55 88 77 99 aa bb cc dd ee ff 00 00 00 00 00 shows up as
        // naa.600224806655887799aaff0000000000.
        let disk = DataDisk {
            path: PathBuf::from(r"D:\WSL\docker-data.vhdx"),
            id: "6655887799aabbccddeeff0000000000".into(),
        };
        assert_eq!(
            disk.wwid().expect("WWID"),
            "600224806655887799aaff0000000000"
        );
        assert!(DataDisk {
            id: "xyz".into(),
            ..disk.clone()
        }
        .wwid()
        .is_err());
        assert_eq!(disk.directory(), PathBuf::from(r"D:\WSL"));
    }

    #[test]
    fn random_ids_are_distinct_guids() {
        let first = random_id(Path::new("a"));
        let second = random_id(Path::new("b"));
        assert_ne!(first, second);
        assert_eq!(first[7] >> 4, 4);
        assert_eq!(first[8] >> 6, 0b10);
    }

    #[test]
    fn state_round_trips_and_moves_files() {
        let directory = tempfile::tempdir().expect("临时目录");
        let root = directory.path().join("root");
        assert_eq!(load(&root).expect("空状态"), None);
        let source_dir = directory.path().join("source");
        fs::create_dir_all(&source_dir).expect("源目录");
        let disk = DataDisk {
            path: source_dir.join(DATA_DISK_FILE),
            id: "0".repeat(32),
        };
        fs::write(&disk.path, b"vhdx").expect("磁盘文件");
        save(&root, &disk).expect("保存");
        assert_eq!(load(&root).expect("读取"), Some(disk.clone()));

        let target_dir = directory.path().join("target");
        let moved = move_file(&disk, &target_dir).expect("移动");
        assert_eq!(moved.path, target_dir.join(DATA_DISK_FILE));
        assert_eq!(moved.id, disk.id);
        assert!(!disk.path.exists());
        assert_eq!(fs::read(&moved.path).expect("读取"), b"vhdx");

        fs::write(&disk.path, b"other").expect("再次创建");
        assert!(matches!(
            move_file(&disk, &target_dir),
            Err(RuntimeHostError::Conflict(_))
        ));
    }

    #[test]
    fn mount_script_only_formats_blank_disks_and_migrates_once() {
        assert!(MOUNT_SCRIPT.contains(r#"if [ "$rc" -eq 2 ]; then"#));
        assert!(MOUNT_SCRIPT.contains("mkfs.ext4 -q -E nodiscard"));
        assert!(MOUNT_SCRIPT.contains(".migrated-$name"));
        assert!(MOUNT_SCRIPT.contains(r#"mount --bind "$DATA/$name" "$target""#));
        // The old data is removed only after the copy and marker succeeded.
        let copy = MOUNT_SCRIPT.find("cp -a").expect("copy");
        let marker = MOUNT_SCRIPT.find(": > ").expect("marker");
        let remove = MOUNT_SCRIPT.find("rm -rf").expect("remove");
        assert!(copy < marker && marker < remove);
    }
}
