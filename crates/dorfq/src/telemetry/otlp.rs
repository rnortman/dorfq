//! OTLP/JSON request bodies: the protobuf messages `ExportMetricsServiceRequest`
//! and `ExportLogsServiceRequest` in the JSON encoding the OTLP/HTTP
//! transport takes. Pure: values in, one body out, the same bytes for the
//! same input.
//!
//! The encoding's rules: every 64-bit integer is a JSON string, a double a
//! JSON number, an enum its integer; a field dorfq does not set is omitted,
//! never `null`. Attributes, metrics, points and records keep the order
//! given.

use std::fmt::Display;

use serde::{Serialize, Serializer};

/// The entity the telemetry describes.
#[derive(Debug, Clone, PartialEq)]
pub struct Resource {
    pub attributes: Vec<(String, Attr)>,
}

/// An attribute's value.
#[derive(Debug, Clone, PartialEq)]
pub enum Attr {
    Str(String),
    Int(i64),
}

/// One metric and its points.
#[derive(Debug, Clone, PartialEq)]
pub struct Metric {
    pub name: String,
    pub description: String,
    pub unit: String,
    pub data: Data,
}

/// A metric's kind and points.
#[derive(Debug, Clone, PartialEq)]
pub enum Data {
    Gauge(Vec<Point>),
    /// Cumulative, monotonic: `start_unix_nano` is the series' start, host
    /// nanoseconds.
    Sum {
        start_unix_nano: u64,
        points: Vec<Point>,
    },
}

/// One data point.
#[derive(Debug, Clone, PartialEq)]
pub struct Point {
    pub attributes: Vec<(String, Attr)>,
    pub time_unix_nano: u64,
    pub value: Number,
}

/// A data point's value.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Number {
    Int(i64),
    Double(f64),
}

/// One log record, its body a string.
#[derive(Debug, Clone, PartialEq)]
pub struct LogRecord {
    pub time_unix_nano: u64,
    pub observed_time_unix_nano: u64,
    pub severity_number: u8,
    pub severity_text: &'static str,
    pub body: String,
    pub attributes: Vec<(String, Attr)>,
}

/// The body of a POST to `/v1/metrics`: `metrics` under one resource and
/// one instrumentation scope named `scope`.
pub fn metrics_request(resource: &Resource, scope: &str, metrics: &[Metric]) -> String {
    let request = MetricsRequest {
        resource_metrics: [ResourceMetrics {
            resource: WireResource::new(resource),
            scope_metrics: [ScopeMetrics {
                scope: Scope { name: scope },
                metrics: metrics.iter().map(WireMetric::new).collect(),
            }],
        }],
    };
    serde_json::to_string(&request).expect("a metrics request serialises")
}

/// The body of a POST to `/v1/logs`: `records` under one resource and one
/// instrumentation scope named `scope`.
pub fn logs_request(resource: &Resource, scope: &str, records: &[LogRecord]) -> String {
    let request = LogsRequest {
        resource_logs: [ResourceLogs {
            resource: WireResource::new(resource),
            scope_logs: [ScopeLogs {
                scope: Scope { name: scope },
                log_records: records.iter().map(WireLogRecord::new).collect(),
            }],
        }],
    };
    serde_json::to_string(&request).expect("a logs request serialises")
}

/// An integer written as a JSON string, as the encoding writes every 64-bit
/// integer.
#[derive(Debug, Clone, Copy)]
struct Decimal<T>(T);

impl<T: Display> Serialize for Decimal<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(&self.0)
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct MetricsRequest<'a> {
    resource_metrics: [ResourceMetrics<'a>; 1],
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ResourceMetrics<'a> {
    resource: WireResource<'a>,
    scope_metrics: [ScopeMetrics<'a>; 1],
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ScopeMetrics<'a> {
    scope: Scope<'a>,
    metrics: Vec<WireMetric<'a>>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LogsRequest<'a> {
    resource_logs: [ResourceLogs<'a>; 1],
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ResourceLogs<'a> {
    resource: WireResource<'a>,
    scope_logs: [ScopeLogs<'a>; 1],
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ScopeLogs<'a> {
    scope: Scope<'a>,
    log_records: Vec<WireLogRecord<'a>>,
}

#[derive(Serialize)]
struct WireResource<'a> {
    attributes: Vec<KeyValue<'a>>,
}

impl<'a> WireResource<'a> {
    fn new(resource: &'a Resource) -> WireResource<'a> {
        WireResource {
            attributes: key_values(&resource.attributes),
        }
    }
}

#[derive(Serialize)]
struct Scope<'a> {
    name: &'a str,
}

#[derive(Serialize)]
struct KeyValue<'a> {
    key: &'a str,
    value: AnyValue<'a>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
enum AnyValue<'a> {
    StringValue(&'a str),
    IntValue(Decimal<i64>),
}

impl<'a> AnyValue<'a> {
    fn new(attr: &'a Attr) -> AnyValue<'a> {
        match attr {
            Attr::Str(s) => AnyValue::StringValue(s),
            Attr::Int(n) => AnyValue::IntValue(Decimal(*n)),
        }
    }
}

fn key_values(attributes: &[(String, Attr)]) -> Vec<KeyValue<'_>> {
    attributes
        .iter()
        .map(|(key, value)| KeyValue {
            key,
            value: AnyValue::new(value),
        })
        .collect()
}

#[derive(Serialize)]
struct WireMetric<'a> {
    name: &'a str,
    description: &'a str,
    unit: &'a str,
    #[serde(flatten)]
    data: WireData<'a>,
}

impl<'a> WireMetric<'a> {
    fn new(metric: &'a Metric) -> WireMetric<'a> {
        let data = match &metric.data {
            Data::Gauge(points) => WireData::Gauge {
                data_points: data_points(points, None),
            },
            Data::Sum {
                start_unix_nano,
                points,
            } => WireData::Sum {
                data_points: data_points(points, Some(*start_unix_nano)),
                aggregation_temporality: AGGREGATION_TEMPORALITY_CUMULATIVE,
                is_monotonic: true,
            },
        };
        WireMetric {
            name: &metric.name,
            description: &metric.description,
            unit: &metric.unit,
            data,
        }
    }
}

/// `AggregationTemporality.AGGREGATION_TEMPORALITY_CUMULATIVE`.
const AGGREGATION_TEMPORALITY_CUMULATIVE: u8 = 2;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
enum WireData<'a> {
    #[serde(rename_all = "camelCase")]
    Gauge { data_points: Vec<DataPoint<'a>> },
    #[serde(rename_all = "camelCase")]
    Sum {
        data_points: Vec<DataPoint<'a>>,
        aggregation_temporality: u8,
        is_monotonic: bool,
    },
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DataPoint<'a> {
    attributes: Vec<KeyValue<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    start_time_unix_nano: Option<Decimal<u64>>,
    time_unix_nano: Decimal<u64>,
    #[serde(flatten)]
    value: WireNumber,
}

fn data_points(points: &[Point], start: Option<u64>) -> Vec<DataPoint<'_>> {
    points
        .iter()
        .map(|point| DataPoint {
            attributes: key_values(&point.attributes),
            start_time_unix_nano: start.map(Decimal),
            time_unix_nano: Decimal(point.time_unix_nano),
            value: match point.value {
                Number::Int(n) => WireNumber::AsInt(Decimal(n)),
                Number::Double(x) => WireNumber::AsDouble(x),
            },
        })
        .collect()
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
enum WireNumber {
    AsInt(Decimal<i64>),
    AsDouble(f64),
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct WireLogRecord<'a> {
    time_unix_nano: Decimal<u64>,
    observed_time_unix_nano: Decimal<u64>,
    severity_number: u8,
    severity_text: &'a str,
    body: AnyValue<'a>,
    attributes: Vec<KeyValue<'a>>,
}

impl<'a> WireLogRecord<'a> {
    fn new(record: &'a LogRecord) -> WireLogRecord<'a> {
        WireLogRecord {
            time_unix_nano: Decimal(record.time_unix_nano),
            observed_time_unix_nano: Decimal(record.observed_time_unix_nano),
            severity_number: record.severity_number,
            severity_text: record.severity_text,
            body: AnyValue::StringValue(&record.body),
            attributes: key_values(&record.attributes),
        }
    }
}
