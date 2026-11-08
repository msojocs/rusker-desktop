//! Opt-in, bounded performance tracing. Set `RUSKER_PERF_LOG` to a JSONL path
//! before starting the app. No container names, paths, payloads or credentials
//! are recorded. File I/O runs on a dedicated thread, never the UI/Tokio worker.

use std::{
    fs::OpenOptions,
    io::{BufWriter, Write},
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc::{sync_channel, SyncSender},
        OnceLock,
    },
    time::{Instant, SystemTime, UNIX_EPOCH},
};

#[derive(serde::Serialize)]
struct Measurement {
    id: u64,
    stage: &'static str,
    started_unix_ms: u128,
    elapsed_us: u128,
}

fn writer() -> Option<&'static SyncSender<Measurement>> {
    static WRITER: OnceLock<Option<SyncSender<Measurement>>> = OnceLock::new();
    WRITER
        .get_or_init(|| {
            let path = std::env::var_os("RUSKER_PERF_LOG").filter(|path| !path.is_empty())?;
            let file = match OpenOptions::new().create(true).append(true).open(path) {
                Ok(file) => file,
                Err(error) => {
                    eprintln!("无法打开性能日志：{error}");
                    return None;
                }
            };
            let (sender, receiver) = sync_channel::<Measurement>(1024);
            std::thread::Builder::new()
                .name("rusker-perf-log".into())
                .spawn(move || {
                    let mut writer = BufWriter::new(file);
                    for measurement in receiver {
                        if serde_json::to_writer(&mut writer, &measurement).is_err()
                            || writer.write_all(b"\n").is_err()
                            || writer.flush().is_err()
                        {
                            break;
                        }
                    }
                })
                .ok()?;
            Some(sender)
        })
        .as_ref()
}

/// RAII timing for a fixed, non-sensitive operation name. Disabled by default.
/// A full log queue drops measurements rather than delaying application work.
pub struct PerfSpan {
    measurement: Option<(&'static SyncSender<Measurement>, Measurement, Instant)>,
}

impl PerfSpan {
    pub fn new(stage: &'static str) -> Self {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        Self {
            measurement: writer().map(|writer| {
                (
                    writer,
                    Measurement {
                        id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
                        stage,
                        started_unix_ms: SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_millis(),
                        elapsed_us: 0,
                    },
                    Instant::now(),
                )
            }),
        }
    }
}

impl Drop for PerfSpan {
    fn drop(&mut self) {
        if let Some((writer, mut measurement, start)) = self.measurement.take() {
            measurement.elapsed_us = start.elapsed().as_micros();
            let _ = writer.try_send(measurement);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measurements_have_only_timing_and_fixed_stage_fields() {
        let value = serde_json::to_value(Measurement {
            id: 1,
            stage: "directory.exec.create",
            started_unix_ms: 1234,
            elapsed_us: 500,
        })
        .unwrap();
        assert_eq!(value.as_object().unwrap().len(), 4);
        assert_eq!(value["stage"], "directory.exec.create");
        assert_eq!(value["elapsed_us"], 500);
    }
}
