// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Translation from [`evo_observatory::Observation`] to the
//! OpenTelemetry SDK's [`SpanData`].
//!
//! Each observation becomes one OTel span. For span-closer
//! observations (`latency_us > 0`) the span has a real
//! duration; for markers and `Started` observations the span
//! is zero-duration (start == end). Span name = the
//! `ObservationKind` discriminator. Attributes pack every
//! field of the observation so operators can filter the OTLP
//! collector by the same dimensions the observatory exposes.

use evo_observatory::attr::AttrValue;
use evo_observatory::cause::DeclineCause;
use evo_observatory::observation::{Observation, Outcome};
use opentelemetry::trace::{
    SpanContext, SpanId as OtelSpanId, SpanKind, Status, TraceFlags, TraceId,
    TraceState,
};
use opentelemetry::{KeyValue, Value};
use opentelemetry_sdk::trace::{SpanData, SpanEvents, SpanLinks};
use opentelemetry_sdk::Resource;
use std::borrow::Cow;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The instrumentation library identifier the exporter
/// stamps every span with. Operators see this on the OTel
/// `InstrumentationScope`; collectors that filter or route by
/// scope name can match on this string.
pub const INSTRUMENTATION_SCOPE_NAME: &str = "evo-observatory";

/// Instrumentation scope version. Tied to the workspace
/// version through cargo's `CARGO_PKG_VERSION` so the value
/// reflects the actual evo build the collector receives data
/// from.
pub const INSTRUMENTATION_SCOPE_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Translate one [`Observation`] into an OTel [`SpanData`].
///
/// The `_resource` argument is reserved for future per-span
/// resource enrichment; today the resource attaches at the
/// SDK's `TracerProvider` level. Accepting the Arc here
/// keeps the signature stable for the day the exporter wants
/// to derive a per-observation resource view.
pub fn observation_to_span_data(
    obs: &Observation,
    _resource: Arc<Resource>,
) -> SpanData {
    let trace_id = trace_id_from_span_id(obs.span.trace_root);
    let span_id = otel_span_id_from_evo(obs.span.span_id);
    let parent_span_id = obs
        .span
        .parent_span_id
        .map(otel_span_id_from_evo)
        .unwrap_or(OtelSpanId::INVALID);

    let start_time = system_time_from_ts_ns(obs.ts_ns);
    let end_time = if obs.latency_us > 0 {
        start_time + Duration::from_micros(obs.latency_us)
    } else {
        start_time
    };

    let mut attributes: Vec<KeyValue> = Vec::with_capacity(8);
    if !obs.op_id.is_empty() {
        attributes.push(KeyValue::new("evo.op_id", obs.op_id.clone()));
    }
    if !obs.principal_token_id.is_empty() {
        attributes.push(KeyValue::new(
            "evo.principal_token_id",
            obs.principal_token_id.clone(),
        ));
    }
    attributes.push(KeyValue::new("evo.outcome", outcome_str(obs.outcome)));
    if obs.latency_us > 0 {
        attributes.push(KeyValue::new("evo.latency_us", obs.latency_us as i64));
    }
    if let Some(cause) = obs.cause.as_ref() {
        attributes.extend(cause_attributes(cause));
    }
    for (k, v) in obs.attrs.iter() {
        attributes.push(KeyValue::new(
            format!("evo.attr.{k}"),
            attr_value_to_otel(v),
        ));
    }

    let status = match obs.outcome {
        Outcome::Success => Status::Ok,
        Outcome::Declined => Status::Error {
            description: obs
                .cause
                .as_ref()
                .map(cause_description)
                .unwrap_or_else(|| Cow::Borrowed("declined")),
        },
        Outcome::Started | Outcome::Informational => Status::Unset,
    };

    let span_context = SpanContext::new(
        trace_id,
        span_id,
        TraceFlags::SAMPLED,
        false,
        TraceState::default(),
    );

    SpanData {
        span_context,
        parent_span_id,
        parent_span_is_remote: false,
        span_kind: SpanKind::Internal,
        name: Cow::Owned(obs.kind.as_str().to_string()),
        start_time,
        end_time,
        attributes,
        dropped_attributes_count: 0,
        events: SpanEvents::default(),
        links: SpanLinks::default(),
        status,
        instrumentation_scope: instrumentation_scope(),
    }
}

/// Build the OTel `InstrumentationScope` the exporter brands
/// every span with. Cheap to construct; cloned per span.
pub fn instrumentation_scope() -> opentelemetry::InstrumentationScope {
    opentelemetry::InstrumentationScope::builder(INSTRUMENTATION_SCOPE_NAME)
        .with_version(INSTRUMENTATION_SCOPE_VERSION)
        .build()
}

/// OTel TraceId is 128-bit — exactly the width of evo's
/// `SpanId`. The mapping is a direct big-endian byte copy.
fn trace_id_from_span_id(id: evo_observatory::span::SpanId) -> TraceId {
    let bytes = id.to_u128().to_be_bytes();
    TraceId::from_bytes(bytes)
}

/// OTel SpanId is 64-bit, half the width of evo's `SpanId`.
/// The low 64 bits of the evo span id carry the per-process
/// monotonic counter; they are unique within one process by
/// construction (the high half is the per-process nonce and
/// is identical across all spans produced by one steward).
/// Taking the low 64 bits preserves intra-process uniqueness
/// while crossing the OTel width barrier deterministically.
fn otel_span_id_from_evo(id: evo_observatory::span::SpanId) -> OtelSpanId {
    let low = (id.to_u128() & 0xFFFF_FFFF_FFFF_FFFF) as u64;
    // OTel reserves the all-zero span id as `INVALID`. The
    // observatory's generator already refuses to emit a zero
    // span id (see `span::generate_span_id`); on the off
    // chance an evo span id collides at the low half with
    // zero we coerce it to 1 to preserve the parent-link
    // invariant.
    let low = if low == 0 { 1 } else { low };
    OtelSpanId::from_bytes(low.to_be_bytes())
}

fn system_time_from_ts_ns(ts_ns: u128) -> SystemTime {
    let secs = (ts_ns / 1_000_000_000) as u64;
    let nanos = (ts_ns % 1_000_000_000) as u32;
    UNIX_EPOCH + Duration::new(secs, nanos)
}

fn outcome_str(o: Outcome) -> &'static str {
    match o {
        Outcome::Started => "started",
        Outcome::Success => "success",
        Outcome::Declined => "declined",
        Outcome::Informational => "informational",
    }
}

fn attr_value_to_otel(v: &AttrValue) -> Value {
    match v {
        AttrValue::Str(s) => Value::String(s.clone().into()),
        AttrValue::UInt(n) => {
            // OTel attribute integers are i64; values above
            // i64::MAX surface as a stringified number rather
            // than silently truncate (no information loss).
            if *n <= i64::MAX as u64 {
                Value::I64(*n as i64)
            } else {
                Value::String(n.to_string().into())
            }
        }
        AttrValue::Int(n) => Value::I64(*n),
        AttrValue::Bool(b) => Value::Bool(*b),
        AttrValue::Float(f) => Value::F64(*f),
        AttrValue::StrList(list) => {
            // OTel attributes carry typed arrays; an array of
            // OTel `StringValue`s preserves the list shape.
            let strings: Vec<opentelemetry::StringValue> = list
                .iter()
                .map(|s| opentelemetry::StringValue::from(s.clone()))
                .collect();
            Value::Array(opentelemetry::Array::String(strings))
        }
    }
}

/// Flatten a [`DeclineCause`] variant into a small set of
/// `evo.cause.*` attribute keys. The discriminator is always
/// included as `evo.cause.kind` so operators can filter by
/// the cause family without parsing the description.
fn cause_attributes(cause: &DeclineCause) -> Vec<KeyValue> {
    match cause {
        DeclineCause::Bearer { reason, token_id } => vec![
            KeyValue::new("evo.cause.kind", "bearer"),
            KeyValue::new("evo.cause.reason", format!("{reason:?}")),
            KeyValue::new("evo.cause.token_id", token_id.clone()),
        ],
        DeclineCause::Capability {
            required,
            held,
            op_id,
        } => {
            let held_strings: Vec<opentelemetry::StringValue> =
                held.iter().map(|s| s.clone().into()).collect();
            vec![
                KeyValue::new("evo.cause.kind", "capability"),
                KeyValue::new("evo.cause.required", required.clone()),
                KeyValue::new("evo.cause.op_id", op_id.clone()),
                KeyValue::new(
                    "evo.cause.held",
                    Value::Array(opentelemetry::Array::String(held_strings)),
                ),
            ]
        }
        DeclineCause::TlsHandshake { reason, detail } => vec![
            KeyValue::new("evo.cause.kind", "tls_handshake"),
            KeyValue::new("evo.cause.reason", format!("{reason:?}")),
            KeyValue::new("evo.cause.detail", detail.clone()),
        ],
        DeclineCause::Payload { op_id, detail } => vec![
            KeyValue::new("evo.cause.kind", "payload"),
            KeyValue::new("evo.cause.op_id", op_id.clone()),
            KeyValue::new("evo.cause.detail", detail.clone()),
        ],
        DeclineCause::StewardError { class, detail } => vec![
            KeyValue::new("evo.cause.kind", "steward_error"),
            KeyValue::new("evo.cause.class", class.clone()),
            KeyValue::new("evo.cause.detail", detail.clone()),
        ],
        DeclineCause::DueTo {
            because_of,
            summary,
        } => vec![
            KeyValue::new("evo.cause.kind", "due_to"),
            KeyValue::new("evo.cause.because_of", because_of.to_hex()),
            KeyValue::new("evo.cause.summary", summary.clone()),
        ],
    }
}

fn cause_description(cause: &DeclineCause) -> Cow<'static, str> {
    match cause {
        DeclineCause::Bearer { reason, .. } => {
            Cow::Owned(format!("bearer:{reason:?}"))
        }
        DeclineCause::Capability {
            op_id, required, ..
        } => Cow::Owned(format!("capability:{op_id} requires {required}")),
        DeclineCause::TlsHandshake { reason, .. } => {
            Cow::Owned(format!("tls_handshake:{reason:?}"))
        }
        DeclineCause::Payload { op_id, .. } => {
            Cow::Owned(format!("payload:{op_id}"))
        }
        DeclineCause::StewardError { class, .. } => {
            Cow::Owned(format!("steward_error:{class}"))
        }
        DeclineCause::DueTo { summary, .. } => Cow::Owned(summary.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use evo_observatory::attr::Attributes;
    use evo_observatory::cause::BearerReason;
    use evo_observatory::kind::ObservationKind;
    use evo_observatory::span::SpanContext as EvoSpanCtx;

    fn empty_resource() -> Arc<Resource> {
        Arc::new(Resource::builder_empty().build())
    }

    #[test]
    fn marker_observation_zero_duration_unset_status() {
        let span = EvoSpanCtx::new_root();
        let obs = Observation::now(
            span,
            ObservationKind::Marker,
            Outcome::Informational,
        );
        let sd = observation_to_span_data(&obs, empty_resource());
        assert_eq!(sd.name.as_ref(), "marker");
        assert_eq!(sd.start_time, sd.end_time);
        assert!(matches!(sd.status, Status::Unset));
        assert_eq!(sd.span_kind, SpanKind::Internal);
    }

    #[test]
    fn closer_observation_has_real_duration() {
        let span = EvoSpanCtx::new_root();
        let obs = Observation::now(
            span,
            ObservationKind::ResponseWritten,
            Outcome::Success,
        )
        .with_op_id("describe_capabilities")
        .with_latency_us(750);
        let sd = observation_to_span_data(&obs, empty_resource());
        let dur = sd
            .end_time
            .duration_since(sd.start_time)
            .expect("end >= start");
        assert_eq!(dur, Duration::from_micros(750));
        assert!(matches!(sd.status, Status::Ok));
    }

    #[test]
    fn declined_observation_has_error_status_with_cause_description() {
        let span = EvoSpanCtx::new_root();
        let obs = Observation::now(
            span,
            ObservationKind::BearerTokenRejected,
            Outcome::Declined,
        )
        .with_cause(DeclineCause::Bearer {
            reason: BearerReason::Expired,
            token_id: "tok-123".into(),
        });
        let sd = observation_to_span_data(&obs, empty_resource());
        match sd.status {
            Status::Error { description } => {
                assert!(description.contains("bearer:Expired"));
            }
            other => panic!("expected Status::Error, got {other:?}"),
        }
        let keys: Vec<_> = sd
            .attributes
            .iter()
            .map(|kv| kv.key.as_str().to_string())
            .collect();
        assert!(keys.iter().any(|k| k == "evo.cause.kind"));
        assert!(keys.iter().any(|k| k == "evo.cause.reason"));
        assert!(keys.iter().any(|k| k == "evo.cause.token_id"));
    }

    #[test]
    fn observation_attributes_carry_through() {
        let span = EvoSpanCtx::new_root();
        let obs = Observation::now(
            span,
            ObservationKind::DispatchCompleted,
            Outcome::Success,
        )
        .with_attrs(
            Attributes::new()
                .with("bytes_in", 42u64)
                .with("op_kind", "read"),
        )
        .with_op_id("list_plugins")
        .with_principal_token_id("tok-xyz");
        let sd = observation_to_span_data(&obs, empty_resource());
        let collected: std::collections::HashMap<String, &Value> = sd
            .attributes
            .iter()
            .map(|kv| (kv.key.as_str().to_string(), &kv.value))
            .collect();
        assert!(collected.contains_key("evo.attr.bytes_in"));
        assert!(collected.contains_key("evo.attr.op_kind"));
        assert!(collected.contains_key("evo.op_id"));
        assert!(collected.contains_key("evo.principal_token_id"));
    }

    #[test]
    fn parent_span_id_maps_from_evo_low_64() {
        let parent = EvoSpanCtx::new_root();
        let child = parent.child();
        let obs = Observation::now(
            child,
            ObservationKind::DispatchStarted,
            Outcome::Started,
        );
        let sd = observation_to_span_data(&obs, empty_resource());
        let expected_low =
            (parent.span_id.to_u128() & 0xFFFF_FFFF_FFFF_FFFF) as u64;
        assert_eq!(
            sd.parent_span_id,
            OtelSpanId::from_bytes(expected_low.to_be_bytes())
        );
    }

    #[test]
    fn root_observation_parent_is_invalid_span_id() {
        let span = EvoSpanCtx::new_root();
        let obs = Observation::now(
            span,
            ObservationKind::DispatchStarted,
            Outcome::Started,
        );
        let sd = observation_to_span_data(&obs, empty_resource());
        assert_eq!(sd.parent_span_id, OtelSpanId::INVALID);
    }

    #[test]
    fn trace_id_uses_full_128_bits_of_trace_root() {
        let span = EvoSpanCtx::new_root();
        let obs = Observation::now(
            span,
            ObservationKind::Marker,
            Outcome::Informational,
        );
        let sd = observation_to_span_data(&obs, empty_resource());
        let expected =
            TraceId::from_bytes(span.trace_root.to_u128().to_be_bytes());
        assert_eq!(sd.span_context.trace_id(), expected);
    }
}
