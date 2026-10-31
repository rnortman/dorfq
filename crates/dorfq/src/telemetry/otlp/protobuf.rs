//! OTLP request bodies in the protobuf binary encoding, written by hand from
//! the `opentelemetry-proto` definitions (`common.proto`, `resource.proto`,
//! `metrics.proto`, `logs.proto` and the two service files).
//!
//! The presence rules are proto3's: a singular scalar equal to its default
//! is not written; a `oneof` member is written whenever it is set, zero
//! included; a set embedded message is written even when empty. Fields are
//! written in field-number order and repeated fields in the order given, so
//! the same input gives the same bytes.

use super::{
    AGGREGATION_TEMPORALITY_CUMULATIVE, Attr, Data, LogRecord, Metric, Number, Point, Resource,
};

/// `ExportMetricsServiceRequest`.
pub(super) fn metrics_request(resource: &Resource, scope: &str, metrics: &[Metric]) -> Vec<u8> {
    let mut out = Vec::new();
    // resource_metrics = 1: `ResourceMetrics`
    message(&mut out, 1, |out| {
        // resource = 1
        message(out, 1, |out| write_resource(out, resource));
        // scope_metrics = 2: `ScopeMetrics`
        message(out, 2, |out| {
            // scope = 1
            message(out, 1, |out| write_scope(out, scope));
            // metrics = 2
            for metric in metrics {
                message(out, 2, |out| write_metric(out, metric));
            }
        });
    });
    out
}

/// `ExportLogsServiceRequest`.
pub(super) fn logs_request(resource: &Resource, scope: &str, records: &[LogRecord]) -> Vec<u8> {
    let mut out = Vec::new();
    // resource_logs = 1: `ResourceLogs`
    message(&mut out, 1, |out| {
        // resource = 1
        message(out, 1, |out| write_resource(out, resource));
        // scope_logs = 2: `ScopeLogs`
        message(out, 2, |out| {
            // scope = 1
            message(out, 1, |out| write_scope(out, scope));
            // log_records = 2
            for record in records {
                message(out, 2, |out| write_log_record(out, record));
            }
        });
    });
    out
}

/// `Resource`.
fn write_resource(out: &mut Vec<u8>, resource: &Resource) {
    // attributes = 1
    write_attributes(out, 1, &resource.attributes);
}

/// `InstrumentationScope`.
fn write_scope(out: &mut Vec<u8>, name: &str) {
    // name = 1
    string(out, 1, name);
}

/// One `KeyValue` per attribute, as the repeated field `field`.
fn write_attributes(out: &mut Vec<u8>, field: u32, attributes: &[(String, Attr)]) {
    for (key, value) in attributes {
        message(out, field, |out| {
            // KeyValue.key = 1
            string(out, 1, key);
            // KeyValue.value = 2
            message(out, 2, |out| write_any_value(out, value));
        });
    }
}

/// `AnyValue`.
fn write_any_value(out: &mut Vec<u8>, value: &Attr) {
    match value {
        Attr::Str(s) => write_string_value(out, s),
        // int_value = 3
        Attr::Int(n) => int64(out, 3, *n),
    }
}

/// `AnyValue` holding a string.
fn write_string_value(out: &mut Vec<u8>, s: &str) {
    // string_value = 1, a `oneof` member: written even when empty
    tag(out, 1, LEN);
    bytes(out, s.as_bytes());
}

/// `Metric`.
fn write_metric(out: &mut Vec<u8>, metric: &Metric) {
    // name = 1
    string(out, 1, &metric.name);
    // description = 2
    string(out, 2, &metric.description);
    // unit = 3
    string(out, 3, &metric.unit);
    match &metric.data {
        // gauge = 5: `Gauge`
        Data::Gauge(points) => message(out, 5, |out| {
            // data_points = 1
            for point in points {
                message(out, 1, |out| write_point(out, None, point));
            }
        }),
        // sum = 7: `Sum`
        Data::Sum {
            start_unix_nano,
            points,
        } => message(out, 7, |out| {
            // data_points = 1
            for point in points {
                message(out, 1, |out| {
                    write_point(out, Some(*start_unix_nano), point)
                });
            }
            // aggregation_temporality = 2
            varint(out, 2, u64::from(AGGREGATION_TEMPORALITY_CUMULATIVE));
            // is_monotonic = 3
            varint(out, 3, 1);
        }),
    }
}

/// `NumberDataPoint`; `start` only for a sum.
fn write_point(out: &mut Vec<u8>, start: Option<u64>, point: &Point) {
    if let Some(start) = start {
        // start_time_unix_nano = 2
        fixed64(out, 2, start);
    }
    // time_unix_nano = 3
    fixed64(out, 3, point.time_unix_nano);
    match point.value {
        // as_double = 4
        Number::Double(x) => double(out, 4, x),
        // as_int = 6
        Number::Int(n) => sfixed64(out, 6, n),
    }
    // attributes = 7
    write_attributes(out, 7, &point.attributes);
}

/// `LogRecord`.
fn write_log_record(out: &mut Vec<u8>, record: &LogRecord) {
    // time_unix_nano = 1
    fixed64(out, 1, record.time_unix_nano);
    // severity_number = 2
    varint(out, 2, u64::from(record.severity_number));
    // severity_text = 3
    string(out, 3, record.severity_text);
    // body = 5: `AnyValue`
    message(out, 5, |out| write_string_value(out, &record.body));
    // attributes = 6
    write_attributes(out, 6, &record.attributes);
    // observed_time_unix_nano = 11
    fixed64(out, 11, record.observed_time_unix_nano);
}

/// Wire type 0: a base-128 varint.
const VARINT: u32 = 0;
/// Wire type 1: eight bytes, little-endian.
const I64: u32 = 1;
/// Wire type 2: a varint length, then that many bytes.
const LEN: u32 = 2;

/// A field's key: its number and wire type.
fn tag(out: &mut Vec<u8>, field: u32, wire_type: u32) {
    write_varint(out, u64::from((field << 3) | wire_type));
}

fn write_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8 & 0x7f) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

/// A varint scalar (`uint32`, an enum, a `bool`), omitted when zero.
fn varint(out: &mut Vec<u8>, field: u32, v: u64) {
    if v != 0 {
        tag(out, field, VARINT);
        write_varint(out, v);
    }
}

/// An `int64` `oneof` member: always written, ten bytes when negative.
fn int64(out: &mut Vec<u8>, field: u32, v: i64) {
    tag(out, field, VARINT);
    write_varint(out, v as u64);
}

/// A `fixed64`, omitted when zero.
fn fixed64(out: &mut Vec<u8>, field: u32, v: u64) {
    if v != 0 {
        tag(out, field, I64);
        out.extend_from_slice(&v.to_le_bytes());
    }
}

/// An `sfixed64` `oneof` member: always written.
fn sfixed64(out: &mut Vec<u8>, field: u32, v: i64) {
    tag(out, field, I64);
    out.extend_from_slice(&v.to_le_bytes());
}

/// A `double` `oneof` member: always written, its IEEE 754 bits as they are.
fn double(out: &mut Vec<u8>, field: u32, x: f64) {
    tag(out, field, I64);
    out.extend_from_slice(&x.to_le_bytes());
}

/// A `string`, omitted when empty.
fn string(out: &mut Vec<u8>, field: u32, s: &str) {
    if !s.is_empty() {
        tag(out, field, LEN);
        bytes(out, s.as_bytes());
    }
}

/// A set embedded message: always written, empty included.
fn message(out: &mut Vec<u8>, field: u32, encode: impl FnOnce(&mut Vec<u8>)) {
    let mut child = Vec::new();
    encode(&mut child);
    tag(out, field, LEN);
    bytes(out, &child);
}

/// A length-delimited payload.
fn bytes(out: &mut Vec<u8>, payload: &[u8]) {
    write_varint(out, payload.len() as u64);
    out.extend_from_slice(payload);
}
