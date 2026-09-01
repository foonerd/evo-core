// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Configuration for the OTLP exporter.

use crate::error::OtelExporterError;
use std::time::Duration;

/// Default batch interval. Every `batch_interval` the
/// exporter snapshots the observatory and exports new
/// observations. 5 s is a balance between operator latency
/// (traces appear within 5 s of emission) and collector
/// efficiency (one batched OTLP request per interval beats
/// per-observation chatter).
pub const DEFAULT_BATCH_INTERVAL: Duration = Duration::from_secs(5);

/// Default batch size cap. The exporter never sends more than
/// this many spans in a single OTLP request. Larger batches
/// reduce request overhead but increase the blast radius of a
/// single failed request — 256 is a conservative default that
/// covers the steady-state per-interval throughput of a busy
/// SBC steward (≤ 50 ops/s × 5 s × ~5 observations/op).
pub const DEFAULT_MAX_BATCH_SIZE: usize = 256;

/// Default service name advertised on the OTel `Resource`.
/// Operators override per-deployment via
/// `EVO_OTLP_SERVICE_NAME` (typically to differentiate per-
/// device traces in a fleet).
pub const DEFAULT_SERVICE_NAME: &str = "evo-steward";

/// Construction-time configuration for the OTLP exporter.
#[derive(Debug, Clone)]
pub struct OtelExporterConfig {
    /// OTLP/HTTP-protobuf collector endpoint. Examples:
    ///
    /// - `http://otel-collector:4318` — `try_from_env` will
    ///   normalise this to `http://otel-collector:4318/v1/traces`
    ///   so the exporter posts to the canonical OTLP/HTTP
    ///   traces path. If the operator passes a full path
    ///   already (`http://collector/custom/path`) it is used
    ///   verbatim.
    /// - `https://api.honeycomb.io` — Honeycomb's hosted
    ///   collector (set `EVO_OTLP_HEADERS=x-honeycomb-team=…`
    ///   for auth).
    ///
    /// gRPC OTLP (port 4317) is NOT supported by this build;
    /// HTTP/protobuf is the only transport, deliberately, so
    /// the dependency footprint stays small and the wire is
    /// debuggable with curl.
    pub endpoint: String,

    /// Service name on the OTel `Resource`. Becomes
    /// `service.name` in the exported traces; defaults to
    /// [`DEFAULT_SERVICE_NAME`].
    pub service_name: String,

    /// How often the background task snapshots the
    /// observatory and exports new observations. Defaults
    /// to [`DEFAULT_BATCH_INTERVAL`].
    pub batch_interval: Duration,

    /// Cap on the number of observations exported in one
    /// OTLP request. Defaults to [`DEFAULT_MAX_BATCH_SIZE`].
    pub max_batch_size: usize,

    /// Optional HTTP headers to send with every OTLP
    /// request. Used by SaaS collectors that require an auth
    /// header (Honeycomb, Grafana Cloud, Datadog, etc.).
    /// Parsed from `EVO_OTLP_HEADERS` as a comma-separated
    /// list of `key=value` pairs.
    pub headers: Vec<(String, String)>,
}

impl OtelExporterConfig {
    /// Construct a fresh config with sensible defaults except
    /// the supplied endpoint.
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            service_name: DEFAULT_SERVICE_NAME.to_string(),
            batch_interval: DEFAULT_BATCH_INTERVAL,
            max_batch_size: DEFAULT_MAX_BATCH_SIZE,
            headers: Vec::new(),
        }
    }

    /// Read the exporter configuration from the process
    /// environment. Returns:
    ///
    /// - `Ok(None)` when `EVO_OTLP_ENDPOINT` is unset — the
    ///   operator has not asked for an OTLP exporter.
    /// - `Ok(Some(_))` with a fully-parsed config when the
    ///   endpoint and any companion variables parse cleanly.
    /// - `Err(_)` when the endpoint or any companion is
    ///   present but malformed; refuse to boot rather than
    ///   silently falling back to defaults.
    ///
    /// Companion variables (all optional, all string-typed
    /// in env, parsed strictly):
    ///
    /// - `EVO_OTLP_SERVICE_NAME` — override the default
    ///   `evo-steward` service name on the Resource.
    /// - `EVO_OTLP_BATCH_INTERVAL_MS` — positive integer
    ///   milliseconds between export batches.
    /// - `EVO_OTLP_MAX_BATCH_SIZE` — positive integer cap
    ///   on observations per batch.
    /// - `EVO_OTLP_HEADERS` — comma-separated `key=value`
    ///   list of HTTP headers, e.g.
    ///   `x-honeycomb-team=...,x-honeycomb-dataset=evo`.
    pub fn try_from_env() -> Result<Option<Self>, OtelExporterError> {
        let endpoint = match std::env::var("EVO_OTLP_ENDPOINT") {
            Ok(v) if !v.is_empty() => v,
            _ => return Ok(None),
        };
        // Minimal scheme + host check. Operators that mistype the
        // scheme are the dominant failure mode in practice; the SDK's
        // downstream parse error is opaque (`InvalidUri`) so we
        // surface the precise reason here. A full URL parser would
        // pull a new dependency for no gain — the SDK validates the
        // remaining structure when it builds the reqwest client.
        let scheme_ok =
            endpoint.starts_with("http://") || endpoint.starts_with("https://");
        if !scheme_ok {
            return Err(OtelExporterError::InvalidEndpoint {
                endpoint: endpoint.clone(),
                detail: "missing scheme (expected http:// or https://)".into(),
            });
        }
        let after_scheme =
            endpoint.split_once("://").map(|x| x.1).unwrap_or("");
        if after_scheme.is_empty() || after_scheme.starts_with('/') {
            return Err(OtelExporterError::InvalidEndpoint {
                endpoint: endpoint.clone(),
                detail: "missing host (expected http://host[:port][/path])"
                    .into(),
            });
        }
        // Normalise to the canonical OTLP/HTTP traces path so
        // operators can paste the same endpoint a Jaeger or
        // OTel-Collector quickstart hands them. The SDK does
        // not auto-append; without this we would silently
        // POST to `/` and most collectors would return 404 or
        // route to a wrong handler.
        let endpoint = normalise_endpoint_path(endpoint);

        let service_name = std::env::var("EVO_OTLP_SERVICE_NAME")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_SERVICE_NAME.to_string());

        let batch_interval = match std::env::var("EVO_OTLP_BATCH_INTERVAL_MS")
            .ok()
            .filter(|s| !s.is_empty())
        {
            Some(raw) => match raw.parse::<u64>() {
                Ok(n) if n > 0 => Duration::from_millis(n),
                _ => {
                    return Err(OtelExporterError::InvalidBatchInterval {
                        raw,
                    });
                }
            },
            None => DEFAULT_BATCH_INTERVAL,
        };

        let max_batch_size = match std::env::var("EVO_OTLP_MAX_BATCH_SIZE")
            .ok()
            .filter(|s| !s.is_empty())
        {
            Some(raw) => match raw.parse::<usize>() {
                Ok(n) if n > 0 => n,
                _ => {
                    return Err(OtelExporterError::InvalidBatchSize { raw });
                }
            },
            None => DEFAULT_MAX_BATCH_SIZE,
        };

        let headers = std::env::var("EVO_OTLP_HEADERS")
            .ok()
            .map(|raw| {
                raw.split(',')
                    .filter_map(|kv| {
                        let kv = kv.trim();
                        if kv.is_empty() {
                            return None;
                        }
                        let mut parts = kv.splitn(2, '=');
                        let k = parts.next()?.trim().to_string();
                        let v = parts.next()?.trim().to_string();
                        if k.is_empty() {
                            None
                        } else {
                            Some((k, v))
                        }
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        Ok(Some(Self {
            endpoint,
            service_name,
            batch_interval,
            max_batch_size,
            headers,
        }))
    }
}

/// Normalise an OTLP endpoint URL to the canonical traces
/// path. Behaviour:
///
/// - When the URL has no path beyond the host (e.g.
///   `http://host:port` or `http://host:port/`),
///   `/v1/traces` is appended.
/// - When the URL already ends with `/v1/traces` (with or
///   without a trailing slash), it is returned unchanged.
/// - Any other custom path is preserved verbatim — operators
///   pointing at an unusual receiver get the URL they typed.
fn normalise_endpoint_path(endpoint: String) -> String {
    let (_scheme_part, rest) = match endpoint.split_once("://") {
        Some(p) => p,
        None => return endpoint, // caller already validated scheme
    };
    let path = rest.find('/').map(|i| &rest[i..]).unwrap_or("");
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        let trimmed_endpoint = endpoint.trim_end_matches('/');
        return format!("{trimmed_endpoint}/v1/traces");
    }
    if trimmed.ends_with("/v1/traces") {
        return endpoint;
    }
    endpoint
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // Env-var tests run sequentially because `std::env::set_var` is
    // process-global and tokio's test threads share the process. We
    // use a single mutex to serialise the env-touching cases.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn with_clean_env<R>(body: impl FnOnce() -> R) -> R {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        for v in [
            "EVO_OTLP_ENDPOINT",
            "EVO_OTLP_SERVICE_NAME",
            "EVO_OTLP_BATCH_INTERVAL_MS",
            "EVO_OTLP_MAX_BATCH_SIZE",
            "EVO_OTLP_HEADERS",
        ] {
            std::env::remove_var(v);
        }
        let out = body();
        for v in [
            "EVO_OTLP_ENDPOINT",
            "EVO_OTLP_SERVICE_NAME",
            "EVO_OTLP_BATCH_INTERVAL_MS",
            "EVO_OTLP_MAX_BATCH_SIZE",
            "EVO_OTLP_HEADERS",
        ] {
            std::env::remove_var(v);
        }
        out
    }

    #[test]
    fn unset_endpoint_yields_none() {
        with_clean_env(|| {
            let cfg = OtelExporterConfig::try_from_env().expect("Ok");
            assert!(cfg.is_none());
        });
    }

    #[test]
    fn empty_endpoint_yields_none() {
        with_clean_env(|| {
            std::env::set_var("EVO_OTLP_ENDPOINT", "");
            let cfg = OtelExporterConfig::try_from_env().expect("Ok");
            assert!(cfg.is_none());
        });
    }

    #[test]
    fn endpoint_without_scheme_errors() {
        with_clean_env(|| {
            std::env::set_var("EVO_OTLP_ENDPOINT", "collector:4318");
            let err = OtelExporterConfig::try_from_env().unwrap_err();
            assert!(matches!(err, OtelExporterError::InvalidEndpoint { .. }));
        });
    }

    #[test]
    fn http_endpoint_parses_with_defaults() {
        with_clean_env(|| {
            std::env::set_var(
                "EVO_OTLP_ENDPOINT",
                "http://otel-collector:4318",
            );
            let cfg = OtelExporterConfig::try_from_env()
                .expect("Ok")
                .expect("Some");
            assert_eq!(
                cfg.endpoint, "http://otel-collector:4318/v1/traces",
                "bare authority must be normalised to canonical OTLP path"
            );
            assert_eq!(cfg.service_name, DEFAULT_SERVICE_NAME);
            assert_eq!(cfg.batch_interval, DEFAULT_BATCH_INTERVAL);
            assert_eq!(cfg.max_batch_size, DEFAULT_MAX_BATCH_SIZE);
            assert!(cfg.headers.is_empty());
        });
    }

    #[test]
    fn trailing_slash_endpoint_normalises_to_canonical_path() {
        with_clean_env(|| {
            std::env::set_var("EVO_OTLP_ENDPOINT", "http://collector:4318/");
            let cfg = OtelExporterConfig::try_from_env()
                .expect("Ok")
                .expect("Some");
            assert_eq!(cfg.endpoint, "http://collector:4318/v1/traces");
        });
    }

    #[test]
    fn explicit_v1_traces_path_preserved() {
        with_clean_env(|| {
            std::env::set_var(
                "EVO_OTLP_ENDPOINT",
                "http://collector:4318/v1/traces",
            );
            let cfg = OtelExporterConfig::try_from_env()
                .expect("Ok")
                .expect("Some");
            assert_eq!(cfg.endpoint, "http://collector:4318/v1/traces");
        });
    }

    #[test]
    fn custom_path_preserved_verbatim() {
        with_clean_env(|| {
            std::env::set_var(
                "EVO_OTLP_ENDPOINT",
                "http://collector:4318/custom/route",
            );
            let cfg = OtelExporterConfig::try_from_env()
                .expect("Ok")
                .expect("Some");
            assert_eq!(cfg.endpoint, "http://collector:4318/custom/route");
        });
    }

    #[test]
    fn service_name_override_applies() {
        with_clean_env(|| {
            std::env::set_var("EVO_OTLP_ENDPOINT", "http://c:4318");
            std::env::set_var(
                "EVO_OTLP_SERVICE_NAME",
                "evo-aarch64-validation",
            );
            let cfg = OtelExporterConfig::try_from_env()
                .expect("Ok")
                .expect("Some");
            assert_eq!(cfg.service_name, "evo-aarch64-validation");
        });
    }

    #[test]
    fn batch_interval_override_applies() {
        with_clean_env(|| {
            std::env::set_var("EVO_OTLP_ENDPOINT", "http://c:4318");
            std::env::set_var("EVO_OTLP_BATCH_INTERVAL_MS", "1500");
            let cfg = OtelExporterConfig::try_from_env()
                .expect("Ok")
                .expect("Some");
            assert_eq!(cfg.batch_interval, Duration::from_millis(1500));
        });
    }

    #[test]
    fn zero_batch_interval_rejected() {
        with_clean_env(|| {
            std::env::set_var("EVO_OTLP_ENDPOINT", "http://c:4318");
            std::env::set_var("EVO_OTLP_BATCH_INTERVAL_MS", "0");
            let err = OtelExporterConfig::try_from_env().unwrap_err();
            assert!(matches!(
                err,
                OtelExporterError::InvalidBatchInterval { .. }
            ));
        });
    }

    #[test]
    fn batch_size_override_applies() {
        with_clean_env(|| {
            std::env::set_var("EVO_OTLP_ENDPOINT", "http://c:4318");
            std::env::set_var("EVO_OTLP_MAX_BATCH_SIZE", "128");
            let cfg = OtelExporterConfig::try_from_env()
                .expect("Ok")
                .expect("Some");
            assert_eq!(cfg.max_batch_size, 128);
        });
    }

    #[test]
    fn headers_parse_multiple_kv_pairs() {
        with_clean_env(|| {
            std::env::set_var("EVO_OTLP_ENDPOINT", "https://api.x.io");
            std::env::set_var(
                "EVO_OTLP_HEADERS",
                "x-honeycomb-team=abc, x-dataset=evo",
            );
            let cfg = OtelExporterConfig::try_from_env()
                .expect("Ok")
                .expect("Some");
            assert_eq!(cfg.headers.len(), 2);
            assert_eq!(cfg.headers[0].0, "x-honeycomb-team");
            assert_eq!(cfg.headers[0].1, "abc");
            assert_eq!(cfg.headers[1].0, "x-dataset");
            assert_eq!(cfg.headers[1].1, "evo");
        });
    }

    #[test]
    fn headers_skip_malformed_entries() {
        with_clean_env(|| {
            std::env::set_var("EVO_OTLP_ENDPOINT", "https://api.x.io");
            // No `=` → skipped; empty key → skipped; valid one kept.
            std::env::set_var(
                "EVO_OTLP_HEADERS",
                "no-equals,=just-value,valid=ok,",
            );
            let cfg = OtelExporterConfig::try_from_env()
                .expect("Ok")
                .expect("Some");
            assert_eq!(cfg.headers, vec![("valid".into(), "ok".into())]);
        });
    }
}
