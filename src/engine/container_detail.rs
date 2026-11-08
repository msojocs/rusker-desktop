//! Container-detail backend. No UI state or Docker daemon is needed by the tests.

use std::{
    future::Future,
    time::{Duration, Instant},
};

use bollard::{
    container::LogOutput,
    exec::{StartExecOptions, StartExecResults},
    models::{ContainerCpuStats, ContainerStatsResponse, ExecConfig},
    query_parameters::{ResizeExecOptions, ResizeExecOptionsBuilder, StatsOptionsBuilder},
    Docker,
};
use futures_util::StreamExt as _;
use tokio::{
    io::AsyncWriteExt as _,
    sync::{mpsc, watch},
    task::AbortHandle,
    time::{sleep, timeout},
};

use super::{with_timeout, EngineError};
use crate::{
    model::{VolumeEntry, VolumeEntryKind},
    volume::{DirectoryListing, MAX_DIRECTORY_ENTRIES},
};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const DIRECTORY_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_DIRECTORY_OUTPUT: usize = 8 * 1024 * 1024;
const SHELL_QUEUE: usize = 32;
const SHELL_CHUNK: usize = 4096;
const MAX_SHELL_INPUT: usize = 64 * 1024;
const DEFAULT_SHELL_SIZE: (u16, u16) = (24, 80);

/// A persistent `/bin/sh -i` with a real TTY and `TERM=xterm-256color`.
/// Input/output strings preserve terminal control bytes and ANSI sequences;
/// send raw input (e.g. `\r` to submit, `\x03` for Ctrl-C), not just commands.
/// Output merges stdout/stderr; backend errors and the final exit status are
/// reported as `\n[rusker: ...]\n` messages before the output channel closes.
///
/// Output is backpressured (32 messages, each at most 3 * (4096 + 3) bytes
/// of decoded data). Dropping this value aborts its IO task, closing both attach halves.
/// Closing all input senders requests `exit`; closing output cancels IO too.
/// Docker has no exec-kill endpoint: disconnect is not a process-tree kill,
/// especially if a command ignores EOF or has background descendants.
#[derive(Debug)]
pub struct ContainerShell {
    pub input: mpsc::Sender<String>,
    pub output: mpsc::Receiver<String>,
    /// Latest terminal size `(rows, cols)`, initially `(24, 80)`.
    /// Zero dimensions are ignored; updates coalesce and are applied with a
    /// 15-second timeout. Resize failures are reported on `output`.
    pub resize: watch::Sender<(u16, u16)>,
    io_task: AbortHandle,
}

impl Drop for ContainerShell {
    fn drop(&mut self) {
        self.io_task.abort();
    }
}

/// Linux/Docker-style resource counters. CPU is percent of one core (may
/// exceed 100); absent/reset/invalid sampling information produces `None`.
/// Missing integer counters are zero, and sums saturate instead of wrapping.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ContainerStats {
    pub cpu_percent: Option<f64>,
    pub memory_bytes: u64,
    pub memory_limit: u64,
    pub network_rx: u64,
    pub network_tx: u64,
    pub block_read: u64,
    pub block_write: u64,
    pub pids: u64,
}

/// One background polling cycle; timestamps are captured at sample completion,
/// not when the slowest container in the batch finishes.
pub struct ContainerStatsBatch {
    pub containers: Vec<crate::model::ContainerSummary>,
    pub samples: Vec<(String, Result<super::StatsSample, EngineError>)>,
}

// Paths are positional arguments, never interpolated into either script.
// Metadata is ASCII under LC_ALL=C; filenames end in NUL, not newline.
// GNU and BusyBox stat -c both accept multiple paths, but neither interprets
// \\0 in -c. Read metadata in bounded batches and pair it with the original argv
// names, not stat's quoted/line-delimited %n output. No symlink dereference/du.
// Swapping LF and NUL makes head -n a portable NUL-record limiter (BusyBox head
// has no -z). All bytes, including embedded LF, survive the inverse swap.
// head closes the producer after limit+1 names; pipes add only bounded read-ahead.
// find implementations may still buffer readdir internally; no portable find
// interface can bound those internal filesystem reads or a blocked syscall.
const DIRECTORY_SCRIPT: &str = r#"LC_ALL=C
export LC_ALL
for tool in find stat tr head xargs; do
    command -v "$tool" >/dev/null 2>&1 || {
        printf '%s\n' 'Directory listing requires sh, find, stat -c, tr, head and xargs -0.' >&2
        exit 127
    }
done
cd "$1" || exit 3
# Report traversal failure in-band; a deliberate downstream close also drops
# this marker, whereas an early failure cannot be mistaken for an empty dir.
{ find . -mindepth 1 -maxdepth 1 -print0 2>/dev/null || printf '\000SCAN_ERROR\000'; } |
    tr '\000\012' '\012\000' | head -n "$2" | tr '\000\012' '\012\000' |
    {
        xargs -0 -r -n 128 /bin/sh -c '
            metadata=$(stat -c "%F|%s|%Y|%A" "$@" 2>/dev/null) || exit 4
            printf "%s\n" "$metadata" | {
                for entry do
                    IFS= read -r row || exit 4
                    printf "%s\n%s\000" "$row" "$entry" || exit 4
                done
            }
        ' rusker-stat || printf '%s\n' 'Directory metadata batch failed.' >&2
    } | head -c "$3"
"#;

pub(super) async fn open_shell(client: &Docker, id: &str) -> Result<ContainerShell, EngineError> {
    let action = "打开容器 Shell（需要 /bin/sh）";
    let created = with_timeout(
        action,
        REQUEST_TIMEOUT,
        client.create_exec(id, shell_config()),
    )
    .await?;
    let attached = with_timeout(
        action,
        REQUEST_TIMEOUT,
        client.start_exec(&created.id, Some(shell_attach_options())),
    )
    .await?;
    let StartExecResults::Attached {
        mut output,
        mut input,
    } = attached
    else {
        return Err(EngineError::operation(action, "引擎未返回 attached IO"));
    };
    // Surface startup failures through the opening Result when already known.
    let state = with_timeout(action, REQUEST_TIMEOUT, client.inspect_exec(&created.id)).await?;
    if state.running == Some(false) && state.exit_code.is_some_and(|code| code != 0) {
        return Err(EngineError::operation(
            action,
            format!("/bin/sh -i 启动退出，退出码 {:?}", state.exit_code),
        ));
    }

    let (input_tx, mut input_rx) = mpsc::channel::<String>(SHELL_QUEUE);
    let (output_tx, output_rx) = mpsc::channel::<String>(SHELL_QUEUE);
    let (resize_tx, mut resize_rx) = watch::channel(DEFAULT_SHELL_SIZE);
    let client = client.clone();
    let task = tokio::spawn(async move {
        let result = {
            let writer = async {
                while let Some(command) = input_rx.recv().await {
                    if command.len() > MAX_SHELL_INPUT {
                        return Err(EngineError::operation(
                            "写入容器 Shell",
                            "单次输入超过 64 KiB",
                        ));
                    }
                    with_io_timeout("写入容器 Shell", input.write_all(command.as_bytes())).await?;
                    with_io_timeout("刷新容器 Shell", input.flush()).await?;
                }
                with_io_timeout("关闭容器 Shell", input.write_all(b"\nexit\n")).await?;
                with_io_timeout("关闭容器 Shell", input.shutdown()).await
            };
            let reader = async {
                // stdout and stderr can interleave between bytes of one character.
                let mut stdout = Utf8Decoder::default();
                let mut stderr = Utf8Decoder::default();
                while let Some(frame) = output.next().await {
                    let frame =
                        frame.map_err(|error| EngineError::operation("读取容器 Shell", error))?;
                    let (decoder, bytes) = match &frame {
                        LogOutput::StdErr { message } => (&mut stderr, message),
                        LogOutput::StdOut { message } | LogOutput::Console { message } => {
                            (&mut stdout, message)
                        }
                        LogOutput::StdIn { .. } => continue,
                    };
                    for chunk in bytes.chunks(SHELL_CHUNK) {
                        send_text(&output_tx, decoder.push(chunk, false)).await?;
                    }
                }
                send_text(&output_tx, stdout.push(&[], true)).await?;
                send_text(&output_tx, stderr.push(&[], true)).await?;
                Ok::<_, EngineError>(())
            };
            let resizer = async {
                while resize_rx.changed().await.is_ok() {
                    // Copy before awaiting: never hold the watch lock across IO.
                    let (rows, cols) = *resize_rx.borrow_and_update();
                    if rows == 0 || cols == 0 {
                        continue;
                    }
                    if let Err(error) = with_timeout(
                        "调整容器 Shell 大小",
                        REQUEST_TIMEOUT,
                        client.resize_exec(&created.id, shell_resize_options(rows, cols)),
                    )
                    .await
                    {
                        send_text(&output_tx, format!("\n[rusker: {error}]\n")).await?;
                    }
                }
                // Closing the resize channel must not end the shell session.
                std::future::pending::<Result<(), EngineError>>().await
            };
            tokio::pin!(writer, reader, resizer);
            let result = tokio::select! {
                _ = output_tx.closed() => return,
                result = &mut reader => result,
                result = &mut resizer => result,
                result = &mut writer => {
                    match result {
                        Err(error) => Err(error),
                        Ok(()) => match timeout(REQUEST_TIMEOUT, &mut reader).await {
                            Ok(result) => result,
                            Err(_) => Err(EngineError::Timeout {
                                action: "等待容器 Shell 退出".into(),
                                seconds: REQUEST_TIMEOUT.as_secs(),
                            }),
                        },
                    }
                }
            };
            result
        };
        drop(input);
        drop(output);
        // All IO/resize futures belong to this task; abort/drop cannot leave
        // an independently spawned writer, reader or resizer behind.
        match result {
            Ok(()) => match exec_exit_code(&client, &created.id, "检查容器 Shell 退出状态").await
            {
                Ok(code) => {
                    let _ = output_tx
                        .send(format!("\n[rusker: Shell 已退出，退出码 {code}]\n"))
                        .await;
                }
                Err(error) => {
                    let _ = output_tx.send(format!("\n[rusker: {error}]\n")).await;
                }
            },
            Err(error) => {
                let _ = output_tx.send(format!("\n[rusker: {error}]\n")).await;
            }
        }
    });
    Ok(ContainerShell {
        input: input_tx,
        output: output_rx,
        resize: resize_tx,
        io_task: task.abort_handle(),
    })
}

fn shell_config() -> ExecConfig {
    ExecConfig {
        attach_stdin: Some(true),
        attach_stdout: Some(true),
        attach_stderr: Some(true),
        tty: Some(true),
        console_size: Some(vec![
            usize::from(DEFAULT_SHELL_SIZE.0),
            usize::from(DEFAULT_SHELL_SIZE.1),
        ]),
        cmd: Some(vec!["/bin/sh".into(), "-i".into()]),
        env: Some(vec!["TERM=xterm-256color".into()]),
        ..Default::default()
    }
}

fn shell_attach_options() -> StartExecOptions {
    StartExecOptions {
        tty: true,
        ..attach_options()
    }
}

fn shell_resize_options(rows: u16, cols: u16) -> ResizeExecOptions {
    ResizeExecOptionsBuilder::default()
        .h(i32::from(rows))
        .w(i32::from(cols))
        .build()
}

// Directory output is binary/NUL-delimited and must never use a TTY.
fn attach_options() -> StartExecOptions {
    StartExecOptions {
        detach: false,
        tty: false,
        output_capacity: Some(SHELL_CHUNK),
    }
}

async fn send_text(output: &mpsc::Sender<String>, text: String) -> Result<(), EngineError> {
    if !text.is_empty() {
        output
            .send(text)
            .await
            .map_err(|_| EngineError::operation("读取容器 Shell", "输出通道已关闭"))?;
    }
    Ok(())
}

async fn with_io_timeout<T>(
    action: &str,
    future: impl Future<Output = std::io::Result<T>>,
) -> Result<T, EngineError> {
    match timeout(REQUEST_TIMEOUT, future).await {
        Ok(result) => result.map_err(|error| EngineError::operation(action, error)),
        Err(_) => Err(EngineError::Timeout {
            action: action.into(),
            seconds: REQUEST_TIMEOUT.as_secs(),
        }),
    }
}

async fn exec_exit_code(client: &Docker, id: &str, action: &str) -> Result<i64, EngineError> {
    // EOF and the daemon's process-exit bookkeeping can race slightly.
    match timeout(REQUEST_TIMEOUT, async {
        loop {
            let state = client
                .inspect_exec(id)
                .await
                .map_err(|error| EngineError::operation(action, error))?;
            match state.running {
                Some(true) => sleep(Duration::from_millis(25)).await,
                Some(false) => {
                    return state
                        .exit_code
                        .ok_or_else(|| EngineError::operation(action, "引擎未返回 exec 退出码"))
                }
                None => return Err(EngineError::operation(action, "引擎未返回 exec 运行状态")),
            }
        }
    })
    .await
    {
        Ok(result) => result,
        Err(_) => Err(EngineError::Timeout {
            action: action.into(),
            seconds: REQUEST_TIMEOUT.as_secs(),
        }),
    }
}

pub(super) async fn list_directory(
    client: &Docker,
    id: &str,
    path: &str,
) -> Result<DirectoryListing, EngineError> {
    let _total = crate::diagnostics::PerfSpan::new("directory.total");
    let action = "读取容器目录（需要 sh、find、stat、tr、head 和 xargs）";
    let path = absolute_path(path).map_err(|error| EngineError::operation(action, error))?;
    let result = timeout(DIRECTORY_TIMEOUT, async {
        let create_span = crate::diagnostics::PerfSpan::new("directory.exec.create");
        let created = client
            .create_exec(
                id,
                ExecConfig {
                    attach_stdin: Some(false),
                    attach_stdout: Some(true),
                    attach_stderr: Some(true),
                    tty: Some(false),
                    cmd: Some(vec![
                        "/bin/sh".into(),
                        "-c".into(),
                        DIRECTORY_SCRIPT.into(),
                        "rusker-list".into(),
                        path,
                        (MAX_DIRECTORY_ENTRIES + 1).to_string(),
                        (MAX_DIRECTORY_OUTPUT + 1).to_string(),
                    ]),
                    env: Some(vec!["LC_ALL=C".into()]),
                    ..Default::default()
                },
            )
            .await
            .map_err(|error| EngineError::operation(action, error))?;
        drop(create_span);
        let start_span = crate::diagnostics::PerfSpan::new("directory.exec.start");
        let StartExecResults::Attached { mut output, input } = client
            .start_exec(&created.id, Some(attach_options()))
            .await
            .map_err(|error| EngineError::operation(action, error))?
        else {
            return Err(EngineError::operation(action, "引擎未返回 attached IO"));
        };
        drop(start_span);
        drop(input);
        let output_span = crate::diagnostics::PerfSpan::new("directory.exec.output");
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut output_bytes = 0usize;
        while let Some(frame) = output.next().await {
            let frame = frame.map_err(|error| EngineError::operation(action, error))?;
            let (target, message) = match frame {
                LogOutput::StdErr { message } => (&mut stderr, message),
                LogOutput::StdOut { message } | LogOutput::Console { message } => {
                    (&mut stdout, message)
                }
                LogOutput::StdIn { .. } => continue,
            };
            output_bytes = checked_output_size(output_bytes, message.len())
                .map_err(|error| EngineError::operation(action, error))?;
            target.extend_from_slice(&message);
        }
        drop(output_span);
        let inspect_span = crate::diagnostics::PerfSpan::new("directory.exec.inspect");
        let code = exec_exit_code(client, &created.id, action).await?;
        drop(inspect_span);
        // POSIX sh has no pipefail. Any diagnostic is fatal, including stat or
        // xargs errors hidden by the final head. Never cache a partial success.
        if code != 0 || !stderr.is_empty() {
            return Err(EngineError::operation(
                action,
                format!(
                    "exec 退出码 {code}：{}",
                    crate::format::bound_logs(&String::from_utf8_lossy(&stderr))
                ),
            ));
        }
        parse_directory(&stdout).map_err(|error| EngineError::operation(action, error))
    })
    .await;
    match result {
        Ok(result) => result,
        Err(_) => Err(EngineError::Timeout {
            action: action.into(),
            seconds: DIRECTORY_TIMEOUT.as_secs(),
        }),
    }
}

fn checked_output_size(current: usize, extra: usize) -> Result<usize, &'static str> {
    current
        .checked_add(extra)
        .filter(|size| *size <= MAX_DIRECTORY_OUTPUT)
        .ok_or("目录输出超过 8 MiB 上限")
}

/// Validate a container POSIX path independently of the host OS. Do not
/// canonicalize `..` across symlinks: require callers to use explicit paths.
fn absolute_path(path: &str) -> Result<String, String> {
    if !path.starts_with('/')
        || path.contains('\0')
        || path.split('/').any(|part| part == "." || part == "..")
    {
        return Err("容器路径必须是绝对路径，且不能包含 NUL、. 或 .. 路径段".into());
    }
    let parts: Vec<_> = path.split('/').filter(|part| !part.is_empty()).collect();
    Ok(format!("/{}", parts.join("/")))
}

fn parse_directory(bytes: &[u8]) -> Result<DirectoryListing, String> {
    let _parse = crate::diagnostics::PerfSpan::new("directory.parse");
    checked_output_size(0, bytes.len()).map_err(str::to_string)?;
    let mut listing = DirectoryListing::default();
    if bytes.is_empty() {
        return Ok(listing);
    }
    if !bytes.ends_with(&[0]) {
        return Err("目录输出被截断（缺少 NUL 记录结束符）".into());
    }
    for record in bytes[..bytes.len() - 1].split(|byte| *byte == 0) {
        if listing.entries.len() == MAX_DIRECTORY_ENTRIES {
            listing.truncated = true;
            break;
        }
        let text = std::str::from_utf8(record)
            .map_err(|_| "目录包含非 UTF-8 文件名，无法表示为 VolumeEntry".to_string())?;
        let (metadata, name) = text.split_once('\n').ok_or("无效的 stat 元数据")?;
        let fields: Vec<_> = metadata.split('|').collect();
        if fields.len() != 4 {
            return Err("无效的 stat 元数据字段".into());
        }
        let name = name
            .strip_prefix("./")
            .filter(|name| !name.is_empty() && !name.contains('/') && *name != "." && *name != "..")
            .ok_or("无效的目录项名称")?;
        let kind = match fields[0] {
            "directory" => VolumeEntryKind::Directory,
            "regular file" | "regular empty file" => VolumeEntryKind::File,
            "symbolic link" => VolumeEntryKind::Symlink,
            _ => VolumeEntryKind::Other,
        };
        let size = fields[1].parse::<u64>().map_err(|_| "无效的文件大小")?;
        let modified = fields[2].parse::<i64>().map_err(|_| "无效的修改时间")?;
        listing.entries.push(VolumeEntry {
            name: name.into(),
            // No recursive directory sizing (du); zero means unmeasured.
            size: if kind == VolumeEntryKind::Directory {
                0
            } else {
                size
            },
            kind,
            modified,
            mode: fields[3].into(),
        });
    }
    listing
        .entries
        .sort_by_cached_key(|entry| (entry.name.to_lowercase(), entry.name.clone()));
    Ok(listing)
}

pub(super) async fn sample_all(client: &Docker) -> Result<ContainerStatsBatch, EngineError> {
    use bollard::query_parameters::ListContainersOptionsBuilder;
    let mut containers = with_timeout(
        "读取统计采样容器列表",
        REQUEST_TIMEOUT,
        client.list_containers(Some(
            ListContainersOptionsBuilder::default().all(true).build(),
        )),
    )
    .await?
    .into_iter()
    .map(super::docker::container_dto)
    .collect::<Vec<_>>();
    containers.sort_by_cached_key(|container| container.name.to_lowercase());
    let running_ids: Vec<String> = containers
        .iter()
        .filter(|container| container.is_running())
        .map(|container| container.id.clone())
        .collect();
    let samples = futures_util::stream::iter(running_ids.into_iter().map(|id| {
        let client = client.clone();
        async move {
            let result = stats(&client, &id).await.map(|stats| super::StatsSample {
                at: Instant::now(),
                timestamp: time::OffsetDateTime::now_utc().unix_timestamp(),
                stats,
                connected: false, // Computed by StatsCache against the preceding sample.
            });
            (id, result)
        }
    }))
    .buffer_unordered(16)
    .collect()
    .await;
    Ok(ContainerStatsBatch {
        containers,
        samples,
    })
}

pub(super) async fn stats(client: &Docker, id: &str) -> Result<ContainerStats, EngineError> {
    let action = "读取容器资源统计";
    // one_shot=true skips the previous sample and breaks CPU delta calculation.
    let options = StatsOptionsBuilder::default()
        .stream(false)
        .one_shot(false)
        .build();
    let stream = client.stats(id, Some(options));
    tokio::pin!(stream);
    match timeout(REQUEST_TIMEOUT, stream.next()).await {
        Ok(Some(Ok(sample))) => Ok(stats_from_sample(&sample)),
        Ok(Some(Err(error))) => Err(EngineError::operation(action, error)),
        Ok(None) => Err(EngineError::operation(action, "引擎未返回统计采样")),
        Err(_) => Err(EngineError::Timeout {
            action: action.into(),
            seconds: REQUEST_TIMEOUT.as_secs(),
        }),
    }
}

fn stats_from_sample(sample: &ContainerStatsResponse) -> ContainerStats {
    let mut result = ContainerStats {
        cpu_percent: sample
            .cpu_stats
            .as_ref()
            .zip(sample.precpu_stats.as_ref())
            .and_then(|(now, before)| cpu_percent(now, before)),
        ..Default::default()
    };
    if let Some(memory) = &sample.memory_stats {
        let cache = memory
            .stats
            .as_ref()
            .and_then(|stats| {
                stats
                    .get("total_inactive_file")
                    .or_else(|| stats.get("inactive_file"))
                    .or_else(|| stats.get("cache"))
            })
            .copied()
            .unwrap_or(0);
        result.memory_bytes = memory.usage.unwrap_or(0).saturating_sub(cache);
        result.memory_limit = memory.limit.unwrap_or(0);
    }
    for network in sample
        .networks
        .iter()
        .flat_map(|networks| networks.values())
    {
        result.network_rx = result
            .network_rx
            .saturating_add(network.rx_bytes.unwrap_or(0));
        result.network_tx = result
            .network_tx
            .saturating_add(network.tx_bytes.unwrap_or(0));
    }
    for entry in sample
        .blkio_stats
        .as_ref()
        .and_then(|stats| stats.io_service_bytes_recursive.as_ref())
        .into_iter()
        .flatten()
    {
        let value = entry.value.unwrap_or(0);
        match entry.op.as_deref() {
            Some(op) if op.eq_ignore_ascii_case("read") => {
                result.block_read = result.block_read.saturating_add(value)
            }
            Some(op) if op.eq_ignore_ascii_case("write") => {
                result.block_write = result.block_write.saturating_add(value)
            }
            _ => {}
        }
    }
    result.pids = sample
        .pids_stats
        .as_ref()
        .and_then(|stats| stats.current)
        .unwrap_or(0);
    result
}

fn cpu_percent(now: &ContainerCpuStats, before: &ContainerCpuStats) -> Option<f64> {
    let usage = now.cpu_usage.as_ref()?;
    let cpu_delta = usage
        .total_usage?
        .checked_sub(before.cpu_usage.as_ref()?.total_usage?)?;
    let system_before = before.system_cpu_usage?;
    // An all-zero/missing previous sample is not a measured idle interval.
    if system_before == 0 {
        return None;
    }
    let system_delta = now.system_cpu_usage?.checked_sub(system_before)?;
    if system_delta == 0 {
        return None;
    }
    let cpus = now
        .online_cpus
        .filter(|cpus| *cpus > 0)
        .map(u64::from)
        .or_else(|| {
            usage
                .percpu_usage
                .as_ref()
                .and_then(|cpus| u64::try_from(cpus.len()).ok())
                .filter(|cpus| *cpus > 0)
        })?;
    let percent = cpu_delta as f64 / system_delta as f64 * cpus as f64 * 100.0;
    percent.is_finite().then_some(percent)
}

/// Retains at most the incomplete UTF-8 suffix (three bytes) between chunks.
#[derive(Default)]
struct Utf8Decoder {
    pending: Vec<u8>,
}

impl Utf8Decoder {
    fn push(&mut self, bytes: &[u8], eof: bool) -> String {
        self.pending.extend_from_slice(bytes);
        let mut text = String::new();
        let mut consumed = 0;
        while consumed < self.pending.len() {
            match std::str::from_utf8(&self.pending[consumed..]) {
                Ok(valid) => {
                    text.push_str(valid);
                    consumed = self.pending.len();
                }
                Err(error) => {
                    let valid_end = consumed + error.valid_up_to();
                    // from_utf8 has just validated this prefix.
                    text.push_str(
                        std::str::from_utf8(&self.pending[consumed..valid_end])
                            .expect("validated UTF-8 prefix"),
                    );
                    consumed = valid_end;
                    match error.error_len() {
                        Some(length) => {
                            text.push('\u{fffd}');
                            consumed += length;
                        }
                        None if eof => {
                            text.push('\u{fffd}');
                            consumed = self.pending.len();
                        }
                        None => break,
                    }
                }
            }
        }
        self.pending.drain(..consumed);
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample(value: serde_json::Value) -> ContainerStats {
        stats_from_sample(&serde_json::from_value(value).unwrap())
    }

    #[test]
    fn shell_launch_is_persistent_tty_sh() {
        let config = shell_config();
        assert_eq!(config.cmd.as_ref().unwrap(), &["/bin/sh", "-i"]);
        assert_eq!(config.env.as_ref().unwrap(), &["TERM=xterm-256color"]);
        assert_eq!(config.tty, Some(true));
        assert_eq!(config.console_size.as_ref().unwrap(), &[24, 80]);
        assert_eq!(config.attach_stdin, Some(true));
        assert_eq!(config.attach_stdout, Some(true));
        assert_eq!(config.attach_stderr, Some(true));
        let serialized = serde_json::to_value(config).unwrap();
        assert_eq!(serialized["ConsoleSize"], json!([24, 80]));
        assert_eq!(serialized["Tty"], json!(true));
        let start = shell_attach_options();
        assert!(start.tty);
        assert!(!start.detach);
        assert_eq!(start.output_capacity, Some(SHELL_CHUNK));
        assert_eq!(serde_json::to_value(start).unwrap()["Tty"], json!(true));
    }

    #[test]
    fn directory_attach_remains_non_tty() {
        assert!(!attach_options().tty);
        assert!(!attach_options().detach);
    }

    #[test]
    fn shell_resize_uses_rows_as_height_and_cols_as_width() {
        assert_eq!(DEFAULT_SHELL_SIZE, (24, 80));
        let options = shell_resize_options(37, 123);
        assert_eq!(options.h, 37);
        assert_eq!(options.w, 123);
        assert_eq!(
            serde_json::to_value(options).unwrap(),
            json!({"h": 37, "w": 123})
        );
        let options = shell_resize_options(u16::MAX, u16::MAX);
        assert_eq!((options.h, options.w), (65535, 65535));
    }

    #[test]
    fn validates_absolute_posix_paths_without_shell_interpolation() {
        for path in ["", "relative", "./etc", "/a/../b", "/a/./b", "/a\0b"] {
            assert!(absolute_path(path).is_err(), "{path:?}");
        }
        assert_eq!(absolute_path("//a///b/").unwrap(), "/a/b");
        assert_eq!(absolute_path("/").unwrap(), "/");
        let hostile = "/a b/\";$(touch x)\n'|\\";
        assert_eq!(absolute_path(hostile).unwrap(), hostile);
        assert!(DIRECTORY_SCRIPT.contains("cd \"$1\""));
        assert!(!DIRECTORY_SCRIPT.contains(hostile));
        assert!(!DIRECTORY_SCRIPT.contains("du "));
    }

    #[test]
    fn directory_records_preserve_special_names_and_do_not_follow_links() {
        let bytes = b"regular file|42|-10|-rw-r--r--\n./a|b\n\"$x\0directory|4096|2|drwxr-xr-x\n./dir\0symbolic link|7|3|lrwxrwxrwx\n./link\0";
        let listing = parse_directory(bytes).unwrap();
        assert_eq!(listing.entries.len(), 3);
        assert_eq!(listing.entries[0].name, "a|b\n\"$x");
        assert_eq!(listing.entries[0].size, 42);
        assert_eq!(listing.entries[0].modified, -10);
        assert_eq!(listing.entries[1].kind, VolumeEntryKind::Directory);
        assert_eq!(listing.entries[1].size, 0);
        assert_eq!(listing.entries[2].kind, VolumeEntryKind::Symlink);
        assert!(!listing.truncated);
        assert_eq!(parse_directory(b"").unwrap(), DirectoryListing::default());
    }

    #[test]
    fn directory_output_errors_and_limits_are_explicit() {
        for bytes in [
            b"partial".as_slice(),
            b"garbage\0",
            b"regular file|bad|1|-rw\n./a\0",
            b"regular file|1|1|-rw\n./a/b\0",
            b"regular file|1|1|-rw\n./\xff\0",
        ] {
            assert!(parse_directory(bytes).is_err());
        }
        let record = b"regular file|1|1|-rw\n./a\0";
        let exact = record.repeat(MAX_DIRECTORY_ENTRIES);
        assert!(!parse_directory(&exact).unwrap().truncated);
        let listing = parse_directory(&record.repeat(MAX_DIRECTORY_ENTRIES + 1)).unwrap();
        assert_eq!(listing.entries.len(), MAX_DIRECTORY_ENTRIES);
        assert!(listing.truncated);
        assert_eq!(
            checked_output_size(MAX_DIRECTORY_OUTPUT - 1, 1),
            Ok(MAX_DIRECTORY_OUTPUT)
        );
        assert!(checked_output_size(MAX_DIRECTORY_OUTPUT, 1).is_err());
        assert!(checked_output_size(1, usize::MAX).is_err());
    }

    // On Windows opt in with RUSKER_DIRECTORY_TEST_WSL=<distribution>; on
    // Linux these exercise real local GNU/BusyBox applets, never Docker.
    fn run_directory_fixture(
        busybox: bool,
        fixture: &str,
        limit: usize,
        output_limit: usize,
    ) -> Option<std::process::Output> {
        use std::{
            io::Write,
            process::{Command, Stdio},
        };
        let mut command = if cfg!(windows) {
            let distro = std::env::var("RUSKER_DIRECTORY_TEST_WSL").ok()?;
            let mut command = Command::new("wsl.exe");
            command.args(["-d", &distro, "--", "/bin/sh", "-s"]);
            command
        } else {
            let mut command = Command::new("/bin/sh");
            command.arg("-s");
            command
        };
        let setup = if busybox {
            r#"busybox=$(command -v busybox) || exit 77
mkdir "$root/tools"
for tool in find stat tr head xargs; do ln -s "$busybox" "$root/tools/$tool"; done
PATH="$root/tools:$PATH"
"#
        } else {
            ""
        };
        let script = format!(
            r#"set -eu
root=$(mktemp -d)
trap 'rm -rf "$root"' EXIT
mkdir "$root/data"
{setup}
{fixture}
list_directory() {{
{DIRECTORY_SCRIPT}
}}
list_directory "$root/data" {limit} {output_limit}
"#
        );
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(script.as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        if output.status.code() == Some(77) {
            eprintln!("BusyBox not installed; script test skipped");
            return None;
        }
        Some(output)
    }

    #[test]
    fn directory_script_gnu_and_busybox_preserve_names_and_metadata() {
        for busybox in [false, true] {
            let Some(output) = run_directory_fixture(
                busybox,
                r#"cd "$root/data"
printf abc > 'a|b
"$x'
: > '-option'
: > '.hidden'
: > "quote' back\\slash"
: > ' leading trailing '
: > '中文🙂'
: > '
'
mkdir dir
printf nested > dir/child
ln -s dir link
ln -s missing dangling
mkfifo pipe
chmod 640 './-option'
"#,
                MAX_DIRECTORY_ENTRIES + 1,
                MAX_DIRECTORY_OUTPUT + 1,
            ) else {
                continue;
            };
            assert!(output.status.success(), "{:?}", output);
            assert!(output.stderr.is_empty(), "{:?}", output);
            let listing = parse_directory(&output.stdout).unwrap();
            assert_eq!(listing.entries.len(), 11);
            let entry = |name| {
                listing
                    .entries
                    .iter()
                    .find(|entry| entry.name == name)
                    .unwrap()
            };
            assert_eq!(entry("a|b\n\"$x").size, 3);
            assert_eq!(entry("-option").mode, "-rw-r-----");
            assert_eq!(entry(" leading trailing ").size, 0);
            assert_eq!(entry("\n").size, 0);
            assert_eq!(entry("中文🙂").size, 0);
            assert_eq!(entry("quote' back\\slash").kind, VolumeEntryKind::File);
            assert_eq!(entry("dir").size, 0);
            assert_eq!(entry("link").kind, VolumeEntryKind::Symlink);
            assert_eq!(entry("link").size, 3);
            assert_eq!(entry("dangling").kind, VolumeEntryKind::Symlink);
            assert_eq!(entry("pipe").kind, VolumeEntryKind::Other);
            assert!(entry(".hidden").modified > 0);
            assert!(!listing.truncated);
        }
    }

    #[test]
    fn directory_script_limits_entries_before_stat_and_bounds_output() {
        for busybox in [false, true] {
            let Some(output) = run_directory_fixture(
                busybox,
                r#"i=0
while [ "$i" -lt 5200 ]; do : > "$root/data/$i"; i=$((i + 1)); done
# Count stat argv and batches without modifying the directory being listed.
real_stat=$(command -v stat)
mkdir "$root/wrapper"
printf '#!/bin/sh\nprintf "%%s\\n" "$#" >> "$RUSKER_BATCH_LOG"\nexec "$RUSKER_REAL_STAT" "$@"\n' > "$root/wrapper/stat"
chmod +x "$root/wrapper/stat"
export RUSKER_REAL_STAT="$real_stat" RUSKER_BATCH_LOG="$root/batches"
PATH="$root/wrapper:$PATH"
# Validate batch count after listing, emitting only failed assertions on stderr.
trap 'lines=$(wc -l < "$root/batches"); [ "$lines" -le 40 ] || printf "too many stat batches\\n" >&2; rm -rf "$root"' EXIT
"#,
                MAX_DIRECTORY_ENTRIES + 1,
                MAX_DIRECTORY_OUTPUT + 1,
            ) else {
                continue;
            };
            assert!(output.status.success(), "{:?}", output);
            assert!(output.stderr.is_empty(), "{:?}", output);
            assert_eq!(
                output.stdout.iter().filter(|byte| **byte == 0).count(),
                MAX_DIRECTORY_ENTRIES + 1
            );
            let listing = parse_directory(&output.stdout).unwrap();
            assert_eq!(listing.entries.len(), MAX_DIRECTORY_ENTRIES);
            assert!(listing.truncated);

            let output = run_directory_fixture(busybox, ": > \"$root/data/file\"", 10, 7).unwrap();
            assert_eq!(output.stdout.len(), 7);
            assert!(parse_directory(&output.stdout).is_err());
            let output = run_directory_fixture(busybox, "rmdir \"$root/data\"", 10, 1000).unwrap();
            assert!(!output.status.success());
        }
    }

    #[test]
    fn utf8_survives_every_chunk_boundary_and_independent_streams() {
        let text = "hello 中文🙂\n";
        for split in 0..=text.len() {
            let mut decoder = Utf8Decoder::default();
            let mut output = decoder.push(&text.as_bytes()[..split], false);
            assert!(decoder.pending.len() <= 3);
            output.push_str(&decoder.push(&text.as_bytes()[split..], false));
            output.push_str(&decoder.push(&[], true));
            assert_eq!(output, text);
        }
        let mut stdout = Utf8Decoder::default();
        let mut stderr = Utf8Decoder::default();
        assert_eq!(stdout.push(&[0xe4], false), "");
        assert_eq!(stderr.push(b"error", false), "error");
        assert_eq!(stdout.push(&[0xb8, 0xad], false), "中");
        assert_eq!(stdout.push(&[0xff, b'a', 0xf0, 0x9f], false), "\u{fffd}a");
        assert_eq!(stdout.push(&[], true), "\u{fffd}");
        assert!(stdout.pending.is_empty());
    }

    #[test]
    fn shell_output_preserves_terminal_controls_across_chunks() {
        let text = "\x1b[31m中文\x1b[0m\r\n\x03\x08\x00";
        for split in 0..=text.len() {
            let mut decoder = Utf8Decoder::default();
            let output = decoder.push(&text.as_bytes()[..split], false)
                + &decoder.push(&text.as_bytes()[split..], true);
            assert_eq!(output, text);
        }
    }

    #[test]
    fn utf8_invalid_sequences_match_lossy_decode() {
        let bytes = [0xf0, 0x9f, b'a', 0xff, 0xe4, 0xb8, 0xad, 0xc0, 0x80];
        for split in 0..=bytes.len() {
            let mut decoder = Utf8Decoder::default();
            let output =
                decoder.push(&bytes[..split], false) + &decoder.push(&bytes[split..], true);
            assert_eq!(output, String::from_utf8_lossy(&bytes));
        }
    }

    #[test]
    fn cpu_uses_deltas_and_online_or_per_cpu_count() {
        let value = json!({
            "cpu_stats": {"cpu_usage":{"total_usage":150}, "system_cpu_usage":1200, "online_cpus":4},
            "precpu_stats": {"cpu_usage":{"total_usage":100}, "system_cpu_usage":1000}
        });
        assert_eq!(sample(value.clone()).cpu_percent, Some(100.0));
        let mut fallback = value.clone();
        fallback["cpu_stats"]["online_cpus"] = json!(0);
        fallback["cpu_stats"]["cpu_usage"]["percpu_usage"] = json!([1, 2]);
        assert_eq!(sample(fallback).cpu_percent, Some(50.0));
        let mut idle = value;
        idle["cpu_stats"]["cpu_usage"]["total_usage"] = json!(100);
        assert_eq!(sample(idle).cpu_percent, Some(0.0));
    }

    #[test]
    fn cpu_missing_zero_and_reset_samples_are_unavailable() {
        assert_eq!(sample(json!({})), ContainerStats::default());
        let base = json!({
            "cpu_stats": {"cpu_usage":{"total_usage":150}, "system_cpu_usage":1200, "online_cpus":4},
            "precpu_stats": {"cpu_usage":{"total_usage":100}, "system_cpu_usage":1000}
        });
        for (pointer, value) in [
            ("/precpu_stats", json!(null)),
            ("/precpu_stats/system_cpu_usage", json!(0)),
            ("/cpu_stats/system_cpu_usage", json!(1000)),
            ("/cpu_stats/system_cpu_usage", json!(999)),
            ("/cpu_stats/cpu_usage/total_usage", json!(99)),
            ("/cpu_stats/online_cpus", json!(0)),
            ("/cpu_stats/cpu_usage/total_usage", json!(null)),
        ] {
            let mut value_copy = base.clone();
            *value_copy.pointer_mut(pointer).unwrap() = value;
            assert_eq!(sample(value_copy).cpu_percent, None, "{pointer}");
        }
    }

    #[test]
    fn memory_cache_precedence_and_saturating_subtraction() {
        for (stats, expected) in [
            (
                json!({"total_inactive_file":20,"inactive_file":30,"cache":40}),
                80,
            ),
            (json!({"inactive_file":30,"cache":40}), 70),
            (json!({"cache":40}), 60),
            (json!({"cache":101}), 0),
            (json!({}), 100),
        ] {
            let result = sample(json!({"memory_stats":{"usage":100,"limit":200,"stats":stats}}));
            assert_eq!(result.memory_bytes, expected);
            assert_eq!(result.memory_limit, 200);
        }
    }

    #[test]
    fn counters_saturate_ignore_total_and_cpu_remains_finite() {
        let result = sample(json!({
            "cpu_stats":{"cpu_usage":{"total_usage":u64::MAX},"system_cpu_usage":2,"online_cpus":u32::MAX},
            "precpu_stats":{"cpu_usage":{"total_usage":0},"system_cpu_usage":1},
            "networks":{"a":{"rx_bytes":u64::MAX,"tx_bytes":10},"b":{"rx_bytes":1,"tx_bytes":20}},
            "blkio_stats":{"io_service_bytes_recursive":[
                {"op":"Read","value":u64::MAX}, {"op":"read","value":1},
                {"op":"WRITE","value":5}, {"op":"Total","value":100}
            ]},
            "pids_stats":{"current":8}
        }));
        assert!(result.cpu_percent.unwrap().is_finite());
        assert_eq!(result.network_rx, u64::MAX);
        assert_eq!(result.network_tx, 30);
        assert_eq!(result.block_read, u64::MAX);
        assert_eq!(result.block_write, 5);
        assert_eq!(result.pids, 8);
    }

    #[tokio::test]
    async fn shell_drop_aborts_io_even_when_input_sender_is_cloned() {
        let (input, mut input_rx) = mpsc::channel(1);
        let (_output_tx, output) = mpsc::channel(1);
        let (resize, resize_rx) = watch::channel(DEFAULT_SHELL_SIZE);
        let task = tokio::spawn(async move {
            let _resize_rx = resize_rx;
            input_rx.recv().await
        });
        let sender = input.clone();
        let resize_sender = resize.clone();
        let shell = ContainerShell {
            input,
            output,
            resize,
            io_task: task.abort_handle(),
        };
        assert_eq!(*shell.resize.borrow(), (24, 80));
        drop(shell);
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(sender.send("ignored\n".into()).await.is_err());
        assert!(resize_sender.send((30, 100)).is_err());
    }

    #[tokio::test]
    async fn output_is_bounded_and_receiver_close_releases_backpressure() {
        let (tx, mut rx) = mpsc::channel(SHELL_QUEUE);
        for _ in 0..SHELL_QUEUE {
            tx.try_send("x".into()).unwrap();
        }
        assert!(matches!(
            tx.try_send("overflow".into()),
            Err(mpsc::error::TrySendError::Full(_))
        ));
        rx.close();
        assert!(send_text(&tx, "closed".into()).await.is_err());
    }
}
