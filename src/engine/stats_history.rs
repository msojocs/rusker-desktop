//! Bounded background history and append-only copies for an open statistics view.
use std::{
    collections::{HashMap, VecDeque},
    time::{Duration, Instant},
};

use super::ContainerStats;
use crate::model::ContainerSummary;

pub const STATS_SAMPLE_INTERVAL: Duration = Duration::from_secs(10);
const HISTORY_WINDOW: Duration = Duration::from_secs(60 * 60);
// Allow request latency, but never connect across a missed sampling interval.
const CONNECTION_WINDOW: Duration = Duration::from_secs(25);

#[derive(Clone, Debug)]
pub struct StatsSample {
    pub at: Instant,
    pub timestamp: i64,
    pub stats: ContainerStats,
    pub connected: bool,
}

#[derive(Clone, Debug, Default)]
pub struct StatsHistory {
    pub samples: VecDeque<StatsSample>,
    interrupted: bool,
}

impl StatsHistory {
    /// Append without expiring points: an open chart keeps its original history.
    pub fn push(&mut self, stats: ContainerStats, at: Instant, timestamp: i64) {
        let connected = !self.interrupted
            && self.samples.back().is_some_and(|previous| {
                at >= previous.at
                    && at.duration_since(previous.at) <= CONNECTION_WINDOW
                    && stats.network_rx >= previous.stats.network_rx
                    && stats.network_tx >= previous.stats.network_tx
                    && stats.block_read >= previous.stats.block_read
                    && stats.block_write >= previous.stats.block_write
            });
        self.append(StatsSample {
            at,
            timestamp,
            stats,
            connected,
        });
        self.interrupted = false;
    }

    /// Preserve the backend's gap information; never duplicate or reorder points.
    pub fn append(&mut self, sample: StatsSample) {
        if self.samples.back().is_none_or(|last| sample.at > last.at) {
            self.samples.push_back(sample);
        }
    }

    pub fn interrupt(&mut self) {
        self.interrupted = true;
    }

    pub fn prune(&mut self, now: Instant) {
        while self
            .samples
            .front()
            .is_some_and(|sample| now.saturating_duration_since(sample.at) >= HISTORY_WINDOW)
        {
            self.samples.pop_front();
        }
    }
}

#[derive(Default)]
struct CachedStats {
    running: bool,
    history: StatsHistory,
    error: Option<String>,
}

#[derive(Default)]
pub struct StatsCache {
    containers: HashMap<String, CachedStats>,
}

impl StatsCache {
    pub fn history(&self, id: &str) -> Option<&StatsHistory> {
        self.containers.get(id).map(|entry| &entry.history)
    }

    pub fn latest(&self, id: &str) -> Option<&ContainerStats> {
        self.history(id)?.samples.back().map(|sample| &sample.stats)
    }

    pub fn error(&self, id: &str) -> Option<&str> {
        self.containers.get(id)?.error.as_deref()
    }

    /// A complete, authoritative container list. Stopped is not removed.
    pub fn reconcile(&mut self, containers: &[ContainerSummary], now: Instant) {
        let mut previous = std::mem::take(&mut self.containers);
        for container in containers {
            let mut entry = previous.remove(&container.id).unwrap_or_default();
            entry.running = container.is_running();
            if !entry.running {
                entry.history.interrupt();
                entry.error = None;
            }
            entry.history.prune(now);
            self.containers.insert(container.id.clone(), entry);
        }
    }

    /// Also runs when no containers are running, or the engine is disconnected.
    pub fn prune(&mut self, now: Instant) {
        for entry in self.containers.values_mut() {
            entry.history.prune(now);
        }
    }

    pub fn push(
        &mut self,
        id: &str,
        stats: ContainerStats,
        at: Instant,
        timestamp: i64,
    ) -> Option<StatsSample> {
        let entry = self.containers.get_mut(id).filter(|entry| entry.running)?;
        entry.history.push(stats, at, timestamp);
        entry.history.prune(at);
        entry.error = None;
        entry.history.samples.back().cloned()
    }

    pub fn fail(&mut self, id: &str, error: String) {
        if let Some(entry) = self.containers.get_mut(id).filter(|entry| entry.running) {
            entry.error = Some(error);
            entry.history.interrupt();
        }
    }

    pub fn interrupt_all(&mut self) {
        for entry in self.containers.values_mut() {
            entry.history.interrupt();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn container(id: &str, running: bool) -> ContainerSummary {
        ContainerSummary {
            id: id.into(),
            state: if running { "running" } else { "exited" }.into(),
            ..Default::default()
        }
    }

    #[test]
    fn background_cache_keeps_only_the_last_hour() {
        let start = Instant::now();
        let mut cache = StatsCache::default();
        cache.reconcile(&[container("a", true)], start);
        for seconds in (0..=7200).step_by(10) {
            cache.push(
                "a",
                ContainerStats::default(),
                start + Duration::from_secs(seconds),
                seconds as i64,
            );
        }
        let samples = &cache.history("a").unwrap().samples;
        assert_eq!(samples.len(), 360);
        assert_eq!(samples.front().unwrap().timestamp, 3610);
        assert_eq!(samples.back().unwrap().timestamp, 7200);
    }

    #[test]
    fn stopped_history_expires_without_further_samples() {
        let start = Instant::now();
        let mut cache = StatsCache::default();
        cache.reconcile(&[container("a", true)], start);
        for seconds in [0, 10, 20] {
            cache.push(
                "a",
                ContainerStats::default(),
                start + Duration::from_secs(seconds),
                seconds as i64,
            );
        }
        cache.reconcile(&[container("a", false)], start + Duration::from_secs(21));
        assert_eq!(cache.history("a").unwrap().samples.len(), 3);
        assert!(cache
            .push(
                "a",
                ContainerStats::default(),
                start + Duration::from_secs(22),
                22
            )
            .is_none());
        cache.prune(start + HISTORY_WINDOW);
        assert_eq!(cache.history("a").unwrap().samples.len(), 2);
        cache.prune(start + HISTORY_WINDOW + Duration::from_secs(20));
        assert!(cache.history("a").unwrap().samples.is_empty());
    }

    #[test]
    fn removing_a_container_drops_history_and_rejects_late_results() {
        let now = Instant::now();
        let mut cache = StatsCache::default();
        cache.reconcile(&[container("a", true), container("b", true)], now);
        cache.push("a", ContainerStats::default(), now, 0);
        cache.fail("a", "failed".into());
        cache.reconcile(&[container("b", true)], now);
        cache.fail("a", "late failure".into());
        assert!(cache.push("a", ContainerStats::default(), now, 0).is_none());
        assert!(cache.history("a").is_none());
        assert!(cache.error("a").is_none());
        assert!(cache.history("b").is_some());
    }

    #[test]
    fn an_open_view_only_appends_and_reopening_uses_the_bounded_cache() {
        let start = Instant::now();
        let mut cache = StatsCache::default();
        cache.reconcile(&[container("a", true)], start);
        cache.push("a", ContainerStats::default(), start, 0);
        let mut view = cache.history("a").unwrap().clone();
        for seconds in (10..=7200).step_by(10) {
            let sample = cache
                .push(
                    "a",
                    ContainerStats::default(),
                    start + Duration::from_secs(seconds),
                    seconds as i64,
                )
                .unwrap();
            view.append(sample.clone());
            view.append(sample); // A duplicate notification must not duplicate a point.
        }
        assert_eq!(cache.history("a").unwrap().samples.len(), 360);
        assert_eq!(
            cache
                .history("a")
                .unwrap()
                .samples
                .front()
                .unwrap()
                .timestamp,
            3610
        );
        cache.prune(start + Duration::from_secs(10800));
        assert_eq!(view.samples.len(), 721);
        assert_eq!(view.samples.front().unwrap().timestamp, 0);
        let reopened = cache.history("a").unwrap().clone();
        assert!(reopened.samples.is_empty());
    }

    #[test]
    fn stopping_restarting_failures_and_disconnects_break_lines() {
        let start = Instant::now();
        let mut cache = StatsCache::default();
        cache.reconcile(&[container("a", true)], start);
        assert!(
            !cache
                .push("a", ContainerStats::default(), start, 0)
                .unwrap()
                .connected
        );
        assert!(
            cache
                .push(
                    "a",
                    ContainerStats::default(),
                    start + Duration::from_secs(10),
                    10
                )
                .unwrap()
                .connected
        );
        cache.reconcile(&[container("a", false)], start + Duration::from_secs(11));
        cache.fail("a", "late error".into());
        assert!(cache.error("a").is_none());
        cache.reconcile(&[container("a", true)], start + Duration::from_secs(12));
        assert!(
            !cache
                .push(
                    "a",
                    ContainerStats::default(),
                    start + Duration::from_secs(20),
                    20
                )
                .unwrap()
                .connected
        );
        cache.fail("a", "failed".into());
        assert_eq!(cache.error("a"), Some("failed"));
        assert!(
            !cache
                .push(
                    "a",
                    ContainerStats::default(),
                    start + Duration::from_secs(30),
                    30
                )
                .unwrap()
                .connected
        );
        assert!(cache.error("a").is_none());
        cache.interrupt_all();
        assert!(
            !cache
                .push(
                    "a",
                    ContainerStats::default(),
                    start + Duration::from_secs(40),
                    40
                )
                .unwrap()
                .connected
        );
    }

    #[test]
    fn counter_resets_and_long_gaps_break_lines() {
        let start = Instant::now();
        let mut history = StatsHistory::default();
        let stats = ContainerStats {
            network_rx: 100,
            ..Default::default()
        };
        history.push(stats.clone(), start, 0);
        history.push(stats, start + Duration::from_secs(10), 10);
        assert!(history.samples.back().unwrap().connected);
        history.push(
            ContainerStats::default(),
            start + Duration::from_secs(20),
            20,
        );
        assert!(!history.samples.back().unwrap().connected);
        history.push(
            ContainerStats::default(),
            start + Duration::from_secs(60),
            60,
        );
        assert!(!history.samples.back().unwrap().connected);
    }
}
