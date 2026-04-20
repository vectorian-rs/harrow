/// Configuration for Harrow's opt-in observability.
///
/// When an `otlp_*_endpoint` is `Some`, that signal is exported via rolly's
/// OTLP exporter. When all are `None`, only JSON stderr logging is active
/// (local dev mode).
///
/// Allocated once at startup and stored in `Arc<TypeMap>` — zero per-request cost.
#[derive(Clone)]
pub struct O11yConfig {
    pub service_name: String,
    pub service_version: String,
    pub environment: String,
    pub otlp_traces_endpoint: Option<String>,
    pub otlp_logs_endpoint: Option<String>,
    pub otlp_metrics_endpoint: Option<String>,
    pub request_id_header: String,
}

impl Default for O11yConfig {
    fn default() -> Self {
        Self {
            service_name: "harrow".to_string(),
            service_version: env!("CARGO_PKG_VERSION").to_string(),
            environment: "development".to_string(),
            otlp_traces_endpoint: None,
            otlp_logs_endpoint: None,
            otlp_metrics_endpoint: None,
            request_id_header: "x-request-id".to_string(),
        }
    }
}

impl From<O11yConfig> for rolly::TelemetryConfig {
    fn from(config: O11yConfig) -> Self {
        Self {
            service_name: config.service_name,
            service_version: config.service_version,
            environment: config.environment,
            otlp_traces_endpoint: config.otlp_traces_endpoint,
            otlp_logs_endpoint: config.otlp_logs_endpoint,
            otlp_metrics_endpoint: config.otlp_metrics_endpoint,
            log_to_stderr: true,
            use_metrics_interval: None,
            metrics_flush_interval: None,
            sampling_rate: None,
            backpressure_strategy: rolly::BackpressureStrategy::Drop,
            resource_attributes: Vec::new(),
        }
    }
}

impl O11yConfig {
    pub fn service_name(mut self, name: impl Into<String>) -> Self {
        self.service_name = name.into();
        self
    }

    pub fn service_version(mut self, version: impl Into<String>) -> Self {
        self.service_version = version.into();
        self
    }

    pub fn environment(mut self, env: impl Into<String>) -> Self {
        self.environment = env.into();
        self
    }

    pub fn otlp_traces_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.otlp_traces_endpoint = Some(endpoint.into());
        self
    }

    pub fn otlp_logs_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.otlp_logs_endpoint = Some(endpoint.into());
        self
    }

    pub fn otlp_metrics_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.otlp_metrics_endpoint = Some(endpoint.into());
        self
    }

    pub fn request_id_header(mut self, header: impl Into<String>) -> Self {
        self.request_id_header = header.into();
        self
    }
}
