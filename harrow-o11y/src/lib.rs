pub mod o11y_middleware;

/// Configuration for Harrow's built-in observability.
///
/// When an `otlp_*_endpoint` is `Some`, that signal is exported via ro11y's
/// OTLP exporter. When all are `None`, only JSON stderr logging is active
/// (local dev mode).
pub struct O11yConfig {
    pub service_name: &'static str,
    pub service_version: &'static str,
    pub environment: &'static str,
    pub otlp_traces_endpoint: Option<&'static str>,
    pub otlp_logs_endpoint: Option<&'static str>,
    pub otlp_metrics_endpoint: Option<&'static str>,
    pub request_id_header: String,
}

impl Default for O11yConfig {
    fn default() -> Self {
        Self {
            service_name: "harrow",
            service_version: "0.1.0",
            environment: "development",
            otlp_traces_endpoint: None,
            otlp_logs_endpoint: None,
            otlp_metrics_endpoint: None,
            request_id_header: "x-request-id".to_string(),
        }
    }
}

impl O11yConfig {
    pub fn request_id_header(mut self, header: impl Into<String>) -> Self {
        self.request_id_header = header.into();
        self
    }
}
