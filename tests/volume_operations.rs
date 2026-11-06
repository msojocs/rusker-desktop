use std::{
    io::Write as _,
    time::{SystemTime, UNIX_EPOCH},
};

use bollard::{query_parameters::RemoveVolumeOptionsBuilder, Docker};
use rusker_desktop::{engine::EngineService, model::VolumeEntryKind};

/// Opt-in mutating smoke test for volume management. Only touches volumes
/// named `rusker-test-volume-*`; may pull the helper image (busybox) once.
/// On Windows set `DOCKER_HOST` to the engine under test.
#[tokio::test]
async fn manages_dedicated_test_volumes() {
    if std::env::var("RUSKER_DOCKER_OPERATIONS").as_deref() != Ok("1") {
        eprintln!("跳过：设置 RUSKER_DOCKER_OPERATIONS=1 后运行专用卷变更测试");
        return;
    }

    let docker = Docker::connect_with_local_defaults()
        .expect("创建 Docker client")
        .negotiate_version()
        .await
        .expect("连接 Docker");
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("系统时间")
        .as_millis();
    let source = format!("rusker-test-volume-{}-{nonce}", std::process::id());
    let clone = format!("{source}-clone");
    let restored = format!("{source}-restored");
    let service = EngineService::new().expect("创建 Engine runtime");
    let archive_dir = tempfile::tempdir().expect("临时目录");
    let archive = archive_dir.path().join("data.tar");
    std::fs::File::create(&archive)
        .and_then(|mut file| file.write_all(&tar_archive()))
        .expect("写入测试归档");

    let result = async {
        macro_rules! run {
            ($handle:expr) => {
                $handle
                    .await
                    .map_err(|error| error.to_string())?
                    .map_err(|error| error.to_string())?
            };
        }

        let snapshot = run!(service.create_volume(Some(source.clone())));
        assert!(snapshot.volumes.iter().any(|volume| volume.name == source));
        assert!(snapshot
            .volumes
            .iter()
            .find(|volume| volume.name == source)
            .is_some_and(|volume| volume.created > 0));

        // Creating an existing name must fail instead of silently reusing it.
        assert!(service
            .create_volume(Some(source.clone()))
            .await
            .expect("join")
            .is_err());

        run!(service.import_volume(source.clone(), archive.clone()));
        let root = run!(service.list_volume_directory(source.clone(), String::new()));
        let names: Vec<&str> = root.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, [".hidden", "dir", "hello.txt"], "{root:?}");
        let dir = root.entries.iter().find(|e| e.name == "dir").unwrap();
        assert_eq!(dir.kind, VolumeEntryKind::Directory);
        let hello = root.entries.iter().find(|e| e.name == "hello.txt").unwrap();
        assert_eq!(hello.kind, VolumeEntryKind::File);
        assert_eq!(hello.size, 6);
        assert_eq!(hello.mode, "-rw-r--r--");

        let nested = run!(service.list_volume_directory(source.clone(), "dir".into()));
        assert_eq!(nested.entries.len(), 1);
        assert_eq!(nested.entries[0].name, "nested file.txt");
        assert!(service
            .list_volume_directory(source.clone(), "missing".into())
            .await
            .expect("join")
            .is_err());

        run!(service.clone_volume(source.clone(), clone.clone()));
        let cloned = run!(service.list_volume_directory(clone.clone(), String::new()));
        assert_eq!(cloned.entries, root.entries);

        // Export, then import the export into a fresh volume: same content.
        let exported = archive_dir.path().join("export.tar");
        let written = run!(service.export_volume(source.clone(), exported.clone()));
        assert_eq!(
            std::fs::metadata(&exported).expect("导出文件").len(),
            written
        );
        assert!(!archive_dir.path().join("export.tar.partial").exists());
        run!(service.create_volume(Some(restored.clone())));
        run!(service.import_volume(restored.clone(), exported.clone()));
        let restored_root = run!(service.list_volume_directory(restored.clone(), String::new()));
        assert_eq!(restored_root.entries, root.entries);
        let restored_nested = run!(service.list_volume_directory(restored.clone(), "dir".into()));
        assert_eq!(restored_nested.entries, nested.entries);

        run!(service.empty_volume(source.clone()));
        let emptied = run!(service.list_volume_directory(source.clone(), String::new()));
        assert!(emptied.entries.is_empty(), "{emptied:?}");
        // The clone is independent of its source.
        let cloned = run!(service.list_volume_directory(clone.clone(), String::new()));
        assert_eq!(cloned.entries.len(), 3);

        let sizes = run!(service.volume_sizes());
        eprintln!("卷大小（引擎支持时）：{:?}", sizes.get(&clone));
        if let Some(size) = sizes.get(&clone) {
            assert!(*size >= 13, "克隆卷至少包含 13 字节文件内容：{size}");
        }

        run!(service.remove_volume(clone.clone()));
        let (snapshot, failures) = run!(service.remove_volumes(vec![
            source.clone(),
            restored.clone(),
            format!("{source}-missing"),
        ]));
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert!(!snapshot.volumes.iter().any(|volume| {
            volume.name == source || volume.name == clone || volume.name == restored
        }));
        Ok::<(), String>(())
    }
    .await;

    for name in [&clone, &source, &restored] {
        let options = RemoveVolumeOptionsBuilder::default().force(true).build();
        _ = docker.remove_volume(name, Some(options)).await;
    }
    tokio::task::spawn_blocking(move || drop(service))
        .await
        .expect("在 blocking 线程释放 Engine runtime");
    result.expect("专用卷上的创建、导入、浏览、克隆、清空和删除");
}

/// Minimal ustar archive: `hello.txt`, `.hidden`, `dir/nested file.txt`.
fn tar_archive() -> Vec<u8> {
    fn header(name: &str, size: usize, kind: u8, mode: &str) -> [u8; 512] {
        let mut header = [0u8; 512];
        header[..name.len()].copy_from_slice(name.as_bytes());
        header[100..108].copy_from_slice(format!("{mode:0>7}\0").as_bytes());
        header[108..116].copy_from_slice(b"0000000\0");
        header[116..124].copy_from_slice(b"0000000\0");
        header[124..136].copy_from_slice(format!("{size:011o}\0").as_bytes());
        header[136..148].copy_from_slice(b"14533577400\0");
        header[156] = kind;
        header[257..263].copy_from_slice(b"ustar\0");
        header[263..265].copy_from_slice(b"00");
        header[148..156].copy_from_slice(b"        ");
        let checksum: u32 = header.iter().map(|&byte| u32::from(byte)).sum();
        header[148..156].copy_from_slice(format!("{checksum:06o}\0 ").as_bytes());
        header
    }
    let mut archive = Vec::new();
    let mut file = |name: &str, content: &[u8]| {
        archive.extend_from_slice(&header(name, content.len(), b'0', "644"));
        archive.extend_from_slice(content);
        archive.resize(archive.len().div_ceil(512) * 512, 0);
    };
    file("hello.txt", b"hello\n");
    file(".hidden", b"x");
    archive.extend_from_slice(&header("dir/", 0, b'5', "755"));
    let mut nested = |name: &str, content: &[u8]| {
        archive.extend_from_slice(&header(name, content.len(), b'0', "600"));
        archive.extend_from_slice(content);
        archive.resize(archive.len().div_ceil(512) * 512, 0);
    };
    nested("dir/nested file.txt", b"nested");
    archive.extend_from_slice(&[0; 1024]);
    archive
}
