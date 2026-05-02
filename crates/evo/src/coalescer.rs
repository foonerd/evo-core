//! Per-subscriber happenings coalescer.
//!
//! Subscribers declaring a `coalesce` config on
//! `subscribe_happenings` collapse same-label events within a
//! window. The framework computes each happening's labels via the
//! `CoalesceLabels` trait, builds the coalesce key from the
//! subscriber's label list, and groups same-key events into a
//! window-bucket. When the bucket's window expires, the surviving
//! candidate (per the `Latest` / `First` selection) is delivered
//! with `seq` set to the maximum seq of the collapsed group so
//! the cursor stays monotonic.
//!
//! Coalescing is per-subscriber, never bus-wide. The durable
//! `happenings_log` records every event pre-coalesce; durable
//! replay through the `since` cursor sees the firehose. A
//! subscriber reconnecting with the same coalesce config re-applies
//! the window over the replay too, producing a stable view across
//! reconnects.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use evo_plugin_sdk::happenings::CoalesceLabels;
use serde::{Deserialize, Serialize};

use crate::happenings::{Happening, HappeningEnvelope};

/// Maximum permitted coalesce window. Higher values risk hiding
/// meaningful state changes; the framework rejects subscriptions
/// that ask for more.
pub const MAX_COALESCE_WINDOW_MS: u32 = 5_000;

/// Default coalesce window when the subscriber declares
/// `coalesce` but omits `window_ms`.
pub const DEFAULT_COALESCE_WINDOW_MS: u32 = 100;

/// Which payload survives when N happenings collapse.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum CoalesceSelection {
    /// The most recent payload survives. Right for state-report
    /// streams (subscriber wants the freshest state).
    #[default]
    Latest,
    /// The oldest payload survives. Right for transition events
    /// (subscriber wants the transition that started the burst).
    First,
}

/// Subscriber-declared coalesce configuration. Optional on
/// `subscribe_happenings`; absent ⇒ no coalescing (firehose
/// delivery, current behaviour).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoalesceConfig {
    /// Label names to coalesce on. Order does not matter (the key
    /// is constructed deterministically). Common shapes:
    /// `["variant", "plugin", "shelf", "handle_id"]` for per-handle
    /// custody streams; `["variant", "primary_subject_id"]` for
    /// per-subject collapse; `["variant", "plugin", "sensor_id"]`
    /// for per-sensor sensor-plugin streams.
    pub labels: Vec<String>,
    /// Window in milliseconds. Same-key happenings within the
    /// window collapse. Defaults to [`DEFAULT_COALESCE_WINDOW_MS`].
    /// Capped at [`MAX_COALESCE_WINDOW_MS`].
    #[serde(default = "default_window_ms")]
    pub window_ms: u32,
    /// Which payload survives a collapse. Defaults to `Latest`.
    #[serde(default)]
    pub selection: CoalesceSelection,
}

fn default_window_ms() -> u32 {
    DEFAULT_COALESCE_WINDOW_MS
}

impl CoalesceConfig {
    /// Validate and clamp the config. Returns `Err` if the labels
    /// list is empty (subscriber asked for coalesce but did not
    /// declare any keys); window is clamped to
    /// `[1, MAX_COALESCE_WINDOW_MS]`.
    pub fn validate(self) -> Result<Self, &'static str> {
        if self.labels.is_empty() {
            return Err(
                "coalesce.labels must be non-empty when coalesce is set",
            );
        }
        let window_ms = self.window_ms.clamp(1, MAX_COALESCE_WINDOW_MS);
        Ok(Self {
            labels: self.labels,
            window_ms,
            selection: self.selection,
        })
    }

    fn window(&self) -> Duration {
        Duration::from_millis(self.window_ms as u64)
    }
}

/// One pending coalesce bucket.
#[derive(Debug)]
struct Bucket {
    /// When the bucket's window expires; the surviving candidate
    /// is emitted at or after this instant.
    expires_at: Instant,
    /// Maximum seq observed for this bucket. The emitted envelope
    /// carries this value so the cursor advances to the highest
    /// suppressed seq, never retreating.
    seq_max: u64,
    /// Surviving payload per the selection rule.
    candidate: Happening,
}

/// Per-subscriber coalescer state machine.
///
/// The subscriber's recv-path loop calls [`Self::observe`] for
/// every received envelope and [`Self::flush_expired`] whenever
/// the next-deadline timer fires. Bucketed events are emitted via
/// the loop's normal write path; pass-through events (those
/// missing a requested label) are returned immediately from
/// `observe`.
#[derive(Debug)]
pub struct Coalescer {
    config: CoalesceConfig,
    buckets: HashMap<String, Bucket>,
}

impl Coalescer {
    /// Construct a coalescer from a validated config.
    pub fn new(config: CoalesceConfig) -> Self {
        Self {
            config,
            buckets: HashMap::new(),
        }
    }

    /// Process one received envelope.
    ///
    /// Returns:
    ///
    /// - `Some(env)` — pass-through. The happening is missing one
    ///   or more requested labels and cannot be coalesced; emit it
    ///   immediately at its original seq. The framework does NOT
    ///   synthesise null values for missing labels — that would
    ///   incorrectly collapse semantically-different happenings.
    /// - `None` — buffered. The happening was assigned to a
    ///   bucket; the surviving candidate emits when the window
    ///   expires.
    pub fn observe(
        &mut self,
        env: HappeningEnvelope,
    ) -> Option<HappeningEnvelope> {
        let labels = env.happening.labels();
        let key = match build_key(&labels, &self.config.labels) {
            Some(k) => k,
            None => return Some(env),
        };

        match self.buckets.get_mut(&key) {
            None => {
                self.buckets.insert(
                    key,
                    Bucket {
                        expires_at: Instant::now() + self.config.window(),
                        seq_max: env.seq,
                        candidate: env.happening,
                    },
                );
            }
            Some(bucket) => {
                if env.seq > bucket.seq_max {
                    bucket.seq_max = env.seq;
                }
                if matches!(self.config.selection, CoalesceSelection::Latest) {
                    bucket.candidate = env.happening;
                }
                // First selection: leave bucket.candidate unchanged.
            }
        }
        None
    }

    /// Earliest expiration deadline across all pending buckets, or
    /// `None` if no buckets are pending. The subscriber loop
    /// uses this to drive a `tokio::time::sleep_until` arm of the
    /// `select!`.
    pub fn next_deadline(&self) -> Option<Instant> {
        self.buckets.values().map(|b| b.expires_at).min()
    }

    /// Drain every bucket whose window has expired at `now` and
    /// return them as ready-to-deliver envelopes.
    pub fn flush_expired(&mut self, now: Instant) -> Vec<HappeningEnvelope> {
        let expired: Vec<String> = self
            .buckets
            .iter()
            .filter(|(_, b)| b.expires_at <= now)
            .map(|(k, _)| k.clone())
            .collect();
        let mut out = Vec::with_capacity(expired.len());
        for key in expired {
            if let Some(bucket) = self.buckets.remove(&key) {
                out.push(HappeningEnvelope {
                    seq: bucket.seq_max,
                    happening: bucket.candidate,
                });
            }
        }
        out
    }

    /// Drain every pending bucket, regardless of expiration.
    /// Called when the subscription is shutting down so the
    /// consumer sees the surviving candidate of every in-flight
    /// bucket before disconnect.
    pub fn drain(&mut self) -> Vec<HappeningEnvelope> {
        let keys: Vec<String> = self.buckets.keys().cloned().collect();
        let mut out = Vec::with_capacity(keys.len());
        for key in keys {
            if let Some(bucket) = self.buckets.remove(&key) {
                out.push(HappeningEnvelope {
                    seq: bucket.seq_max,
                    happening: bucket.candidate,
                });
            }
        }
        out
    }
}

/// Build the coalesce key from the happening's labels and the
/// subscriber's requested label list.
///
/// Returns `None` if any requested label is missing from the
/// happening's label set — the framework does NOT synthesise
/// null values; missing-label happenings pass through
/// individually.
fn build_key(
    labels: &std::collections::BTreeMap<&'static str, String>,
    requested: &[String],
) -> Option<String> {
    // Sort the requested labels deterministically so two
    // subscribers asking for the same labels in different orders
    // produce the same key shape — though they're independent
    // coalescers in practice, so this is for canonicalisation
    // hygiene rather than correctness.
    let mut sorted: Vec<&String> = requested.iter().collect();
    sorted.sort();
    let mut parts = Vec::with_capacity(sorted.len());
    for label in sorted {
        let value = labels.get(label.as_str())?;
        parts.push(format!("{label}={value}"));
    }
    Some(parts.join("|"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::happenings::Happening;
    use std::time::SystemTime;

    fn fresh_envelope(
        seq: u64,
        plugin: &str,
        handle: &str,
    ) -> HappeningEnvelope {
        HappeningEnvelope {
            seq,
            happening: Happening::CustodyTaken {
                plugin: plugin.to_string(),
                handle_id: handle.to_string(),
                shelf: "x.y".to_string(),
                custody_type: "t".to_string(),
                at: SystemTime::now(),
            },
        }
    }

    #[test]
    fn validate_rejects_empty_labels() {
        let cfg = CoalesceConfig {
            labels: vec![],
            window_ms: 100,
            selection: CoalesceSelection::Latest,
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn validate_clamps_window_to_max() {
        let cfg = CoalesceConfig {
            labels: vec!["variant".into()],
            window_ms: 999_999,
            selection: CoalesceSelection::Latest,
        };
        let v = cfg.validate().unwrap();
        assert_eq!(v.window_ms, MAX_COALESCE_WINDOW_MS);
    }

    #[test]
    fn validate_clamps_window_to_one_ms_minimum() {
        let cfg = CoalesceConfig {
            labels: vec!["variant".into()],
            window_ms: 0,
            selection: CoalesceSelection::Latest,
        };
        let v = cfg.validate().unwrap();
        assert_eq!(v.window_ms, 1);
    }

    #[test]
    fn observe_buckets_same_key() {
        let cfg = CoalesceConfig {
            labels: vec!["variant".into(), "plugin".into()],
            window_ms: 100,
            selection: CoalesceSelection::Latest,
        }
        .validate()
        .unwrap();
        let mut c = Coalescer::new(cfg);

        // Two events from same plugin → same bucket.
        let r1 = c.observe(fresh_envelope(1, "p", "h1"));
        let r2 = c.observe(fresh_envelope(2, "p", "h2"));
        assert!(r1.is_none(), "first event must bucket");
        assert!(r2.is_none(), "second event same key must bucket");
        assert_eq!(c.buckets.len(), 1);
    }

    #[test]
    fn observe_separates_different_keys() {
        let cfg = CoalesceConfig {
            labels: vec!["variant".into(), "handle_id".into()],
            window_ms: 100,
            selection: CoalesceSelection::Latest,
        }
        .validate()
        .unwrap();
        let mut c = Coalescer::new(cfg);

        c.observe(fresh_envelope(1, "p", "h1"));
        c.observe(fresh_envelope(2, "p", "h2"));
        // Distinct handle_id ⇒ distinct buckets.
        assert_eq!(c.buckets.len(), 2);
    }

    #[test]
    fn observe_passes_through_missing_label() {
        let cfg = CoalesceConfig {
            labels: vec!["variant".into(), "nonexistent_label".into()],
            window_ms: 100,
            selection: CoalesceSelection::Latest,
        }
        .validate()
        .unwrap();
        let mut c = Coalescer::new(cfg);

        let env = fresh_envelope(1, "p", "h1");
        let result = c.observe(env);
        assert!(
            result.is_some(),
            "missing label must produce pass-through, not bucket"
        );
        assert_eq!(c.buckets.len(), 0);
    }

    #[test]
    fn observe_latest_selection_overwrites_payload() {
        let cfg = CoalesceConfig {
            labels: vec!["variant".into(), "plugin".into()],
            window_ms: 100,
            selection: CoalesceSelection::Latest,
        }
        .validate()
        .unwrap();
        let mut c = Coalescer::new(cfg);

        c.observe(fresh_envelope(1, "p", "h1"));
        c.observe(fresh_envelope(5, "p", "h-latest"));

        let bucket = c.buckets.values().next().unwrap();
        assert_eq!(bucket.seq_max, 5);
        match &bucket.candidate {
            Happening::CustodyTaken { handle_id, .. } => {
                assert_eq!(handle_id, "h-latest");
            }
            _ => panic!("unexpected variant"),
        }
    }

    #[test]
    fn observe_first_selection_keeps_oldest_payload() {
        let cfg = CoalesceConfig {
            labels: vec!["variant".into(), "plugin".into()],
            window_ms: 100,
            selection: CoalesceSelection::First,
        }
        .validate()
        .unwrap();
        let mut c = Coalescer::new(cfg);

        c.observe(fresh_envelope(1, "p", "h-first"));
        c.observe(fresh_envelope(5, "p", "h-second"));

        let bucket = c.buckets.values().next().unwrap();
        // seq_max still tracks the highest suppressed seq.
        assert_eq!(bucket.seq_max, 5);
        // But the candidate payload is the first-observed one.
        match &bucket.candidate {
            Happening::CustodyTaken { handle_id, .. } => {
                assert_eq!(handle_id, "h-first");
            }
            _ => panic!("unexpected variant"),
        }
    }

    #[test]
    fn flush_expired_removes_only_expired_buckets() {
        let cfg = CoalesceConfig {
            labels: vec!["variant".into(), "handle_id".into()],
            window_ms: 100,
            selection: CoalesceSelection::Latest,
        }
        .validate()
        .unwrap();
        let mut c = Coalescer::new(cfg);

        c.observe(fresh_envelope(1, "p", "h1"));
        c.observe(fresh_envelope(2, "p", "h2"));
        assert_eq!(c.buckets.len(), 2);

        // Flush in the past — nothing expired yet.
        let now = Instant::now();
        let drained = c.flush_expired(now);
        assert!(drained.is_empty());
        assert_eq!(c.buckets.len(), 2);

        // Flush in the future — both buckets expire.
        let later = now + Duration::from_millis(500);
        let drained = c.flush_expired(later);
        assert_eq!(drained.len(), 2);
        assert_eq!(c.buckets.len(), 0);
    }

    #[test]
    fn flushed_envelope_carries_max_seq() {
        let cfg = CoalesceConfig {
            labels: vec!["variant".into(), "plugin".into()],
            window_ms: 100,
            selection: CoalesceSelection::Latest,
        }
        .validate()
        .unwrap();
        let mut c = Coalescer::new(cfg);

        c.observe(fresh_envelope(3, "p", "h"));
        c.observe(fresh_envelope(7, "p", "h"));
        c.observe(fresh_envelope(5, "p", "h"));

        let drained = c.drain();
        assert_eq!(drained.len(), 1);
        assert_eq!(
            drained[0].seq, 7,
            "flushed envelope must carry the maximum suppressed seq, not \
             the first or the latest insert"
        );
    }

    #[test]
    fn drain_returns_every_pending_bucket() {
        let cfg = CoalesceConfig {
            labels: vec!["variant".into(), "handle_id".into()],
            window_ms: 100_000, // never expires under flush_expired
            selection: CoalesceSelection::Latest,
        }
        .validate()
        .unwrap();
        let mut c = Coalescer::new(cfg);

        c.observe(fresh_envelope(1, "p", "h1"));
        c.observe(fresh_envelope(2, "p", "h2"));
        c.observe(fresh_envelope(3, "p", "h3"));

        let drained = c.drain();
        assert_eq!(drained.len(), 3);
        assert_eq!(c.buckets.len(), 0);
    }
}
