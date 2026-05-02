//! Integration tests for `#[derive(CoalesceLabels)]`.
//!
//! Exercises the macro against representative enum shapes — named
//! fields, unit variants, the `skip` attribute, and the `flatten`
//! attribute on a `serde_json::Value` field — and asserts the
//! generated `labels()` and `static_labels()` outputs.

use evo_coalesce_labels::CoalesceLabels;
use evo_plugin_sdk::happenings::CoalesceLabels as CoalesceLabelsTrait;
use serde_json::json;

#[derive(CoalesceLabels)]
#[allow(dead_code)]
enum Sample {
    Plain {
        plugin: String,
        count: u32,
    },
    Unit,
    WithSkip {
        plugin: String,
        #[coalesce_labels(skip)]
        large_blob: Vec<u8>,
    },
    WithFlatten {
        plugin: String,
        event_type: String,
        #[coalesce_labels(flatten)]
        payload: serde_json::Value,
    },
}

#[test]
fn plain_variant_extracts_field_labels() {
    let s = Sample::Plain {
        plugin: "com.example".to_string(),
        count: 42,
    };
    let labels = s.labels();
    assert_eq!(labels.get("variant"), Some(&"plain".to_string()));
    assert_eq!(labels.get("plugin"), Some(&"com.example".to_string()));
    assert_eq!(labels.get("count"), Some(&"42".to_string()));
    assert_eq!(labels.len(), 3);
}

#[test]
fn unit_variant_carries_only_variant_label() {
    let s = Sample::Unit;
    let labels = s.labels();
    assert_eq!(labels.get("variant"), Some(&"unit".to_string()));
    assert_eq!(labels.len(), 1);
}

#[test]
fn skip_attribute_excludes_field_from_labels() {
    let s = Sample::WithSkip {
        plugin: "p".to_string(),
        large_blob: vec![1, 2, 3, 4, 5],
    };
    let labels = s.labels();
    assert_eq!(labels.get("variant"), Some(&"with_skip".to_string()));
    assert_eq!(labels.get("plugin"), Some(&"p".to_string()));
    assert!(
        !labels.contains_key("large_blob"),
        "skip attribute must exclude the field from labels"
    );
}

#[test]
fn flatten_attribute_promotes_payload_keys_to_labels() {
    let s = Sample::WithFlatten {
        plugin: "com.volumio.sensor.cpu_temp".to_string(),
        event_type: "reading".to_string(),
        payload: json!({
            "sensor_id": "cpu",
            "value_celsius": 76.2,
            "unit": "C"
        }),
    };
    let labels = s.labels();
    assert_eq!(labels.get("variant"), Some(&"with_flatten".to_string()));
    assert_eq!(
        labels.get("plugin"),
        Some(&"com.volumio.sensor.cpu_temp".to_string())
    );
    assert_eq!(labels.get("event_type"), Some(&"reading".to_string()));
    assert_eq!(labels.get("sensor_id"), Some(&"cpu".to_string()));
    assert_eq!(labels.get("value_celsius"), Some(&"76.2".to_string()));
    assert_eq!(labels.get("unit"), Some(&"C".to_string()));
}

#[test]
fn flatten_skips_non_object_payloads() {
    let s = Sample::WithFlatten {
        plugin: "p".to_string(),
        event_type: "e".to_string(),
        payload: json!(["array", "not", "object"]),
    };
    let labels = s.labels();
    // Static labels are still present; flattened payload contributes
    // nothing because the value is not an object.
    assert_eq!(labels.get("plugin"), Some(&"p".to_string()));
    assert_eq!(labels.get("event_type"), Some(&"e".to_string()));
    assert!(!labels.contains_key("array"));
}

#[test]
fn static_labels_returns_canonical_set_per_kind() {
    let plain = Sample::static_labels("plain");
    assert_eq!(plain, &["variant", "plugin", "count"]);

    let unit = Sample::static_labels("unit");
    assert_eq!(unit, &["variant"]);

    let skip = Sample::static_labels("with_skip");
    assert_eq!(skip, &["variant", "plugin"]);

    let flatten = Sample::static_labels("with_flatten");
    // Flattened payload labels are NOT enumerated statically.
    assert_eq!(flatten, &["variant", "plugin", "event_type"]);
}

#[test]
fn static_labels_unknown_kind_returns_empty() {
    let unknown = Sample::static_labels("does_not_exist");
    assert!(unknown.is_empty());
}
