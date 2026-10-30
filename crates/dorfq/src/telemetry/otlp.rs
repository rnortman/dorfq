//! OTLP request bodies: the protobuf messages `ExportMetricsServiceRequest`
//! and `ExportLogsServiceRequest`, rendered from one model in either of the
//! two encodings the OTLP/HTTP transport takes — the protobuf binary
//! encoding or JSON. Pure: values in, one body out, the same bytes for the
//! same input.

mod json;
mod protobuf;

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

/// The OTLP/HTTP encoding of a request body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    /// `application/x-protobuf`: the encoding every OTLP/HTTP receiver accepts.
    Protobuf,
    /// `application/json`.
    Json,
}

impl Encoding {
    /// The `Content-Type` a body in this encoding is posted as.
    pub fn content_type(self) -> &'static str {
        match self {
            Encoding::Protobuf => "application/x-protobuf",
            Encoding::Json => "application/json",
        }
    }
}

/// `AggregationTemporality.AGGREGATION_TEMPORALITY_CUMULATIVE`.
const AGGREGATION_TEMPORALITY_CUMULATIVE: u8 = 2;

/// The body of a POST to `/v1/metrics`: `metrics` under one resource and
/// one instrumentation scope named `scope`.
pub fn metrics_request(
    encoding: Encoding,
    resource: &Resource,
    scope: &str,
    metrics: &[Metric],
) -> Vec<u8> {
    match encoding {
        Encoding::Protobuf => protobuf::metrics_request(resource, scope, metrics),
        Encoding::Json => json::metrics_request(resource, scope, metrics),
    }
}

/// The body of a POST to `/v1/logs`: `records` under one resource and one
/// instrumentation scope named `scope`.
pub fn logs_request(
    encoding: Encoding,
    resource: &Resource,
    scope: &str,
    records: &[LogRecord],
) -> Vec<u8> {
    match encoding {
        Encoding::Protobuf => protobuf::logs_request(resource, scope, records),
        Encoding::Json => json::logs_request(resource, scope, records),
    }
}
