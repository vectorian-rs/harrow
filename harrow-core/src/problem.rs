use std::collections::BTreeMap;

use http::StatusCode;

use crate::response::{IntoResponse, Response};

/// RFC 9457 problem detail response builder.
///
/// Produces `application/problem+json` responses without requiring a separate
/// error framework. Extension members are string-valued for simplicity.
#[derive(Clone, Debug)]
pub struct ProblemDetail {
    status: StatusCode,
    type_uri: Option<String>,
    title: Option<String>,
    detail: Option<String>,
    instance: Option<String>,
    extensions: BTreeMap<String, String>,
}

impl ProblemDetail {
    /// Create a problem detail response for the given status code.
    pub fn new(status: StatusCode) -> Self {
        Self {
            status,
            type_uri: None,
            title: None,
            detail: None,
            instance: None,
            extensions: BTreeMap::new(),
        }
    }

    /// Set the RFC 9457 `type` URI. Defaults to `about:blank`.
    pub fn type_uri(mut self, uri: impl Into<String>) -> Self {
        self.type_uri = Some(uri.into());
        self
    }

    /// Set the human-readable title. Defaults to the HTTP reason phrase.
    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    /// Set the human-readable detail string.
    pub fn detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    /// Set the request-specific `instance` URI.
    pub fn instance(mut self, instance: impl Into<String>) -> Self {
        self.instance = Some(instance.into());
        self
    }

    /// Add an extension member.
    ///
    /// Reserved names from RFC 9457 are ignored during serialization.
    pub fn extension(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.extensions.insert(key.into(), value.into());
        self
    }

    fn body_json(&self) -> String {
        let mut out = String::from("{");
        let mut first = true;

        append_json_field(
            &mut out,
            &mut first,
            "type",
            self.type_uri.as_deref().unwrap_or("about:blank"),
        );
        append_json_field(
            &mut out,
            &mut first,
            "title",
            self.title
                .as_deref()
                .unwrap_or_else(|| self.status.canonical_reason().unwrap_or("Unknown Status")),
        );
        append_json_number(&mut out, &mut first, "status", self.status.as_u16());

        if let Some(detail) = &self.detail {
            append_json_field(&mut out, &mut first, "detail", detail);
        }
        if let Some(instance) = &self.instance {
            append_json_field(&mut out, &mut first, "instance", instance);
        }

        for (key, value) in &self.extensions {
            if matches!(
                key.as_str(),
                "type" | "title" | "status" | "detail" | "instance"
            ) {
                continue;
            }
            append_json_field(&mut out, &mut first, key, value);
        }

        out.push('}');
        out
    }
}

impl IntoResponse for ProblemDetail {
    fn into_response(self) -> Response {
        Response::new(self.status, self.body_json())
            .header("content-type", "application/problem+json")
    }
}

fn append_json_field(out: &mut String, first: &mut bool, key: &str, value: &str) {
    if !*first {
        out.push(',');
    }
    *first = false;
    push_json_string(out, key);
    out.push(':');
    push_json_string(out, value);
}

fn append_json_number(out: &mut String, first: &mut bool, key: &str, value: u16) {
    if !*first {
        out.push(',');
    }
    *first = false;
    push_json_string(out, key);
    out.push(':');
    out.push_str(&value.to_string());
}

fn push_json_string(out: &mut String, value: &str) {
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0C}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => {
                let escaped = format!("\\u{:04x}", c as u32);
                out.push_str(&escaped);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;

    async fn body_string(resp: Response) -> String {
        let body = resp
            .into_inner()
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes();
        String::from_utf8(body.to_vec()).unwrap()
    }

    #[tokio::test]
    async fn problem_detail_defaults_reason_phrase_and_about_blank() {
        let resp = ProblemDetail::new(StatusCode::NOT_FOUND).into_response();
        assert_eq!(resp.status_code(), StatusCode::NOT_FOUND);
        assert_eq!(
            resp.inner().headers().get("content-type").unwrap(),
            "application/problem+json"
        );

        let body = body_string(resp).await;
        assert!(body.contains("\"type\":\"about:blank\""));
        assert!(body.contains("\"title\":\"Not Found\""));
        assert!(body.contains("\"status\":404"));
    }

    #[tokio::test]
    async fn problem_detail_serializes_optional_fields_and_extensions() {
        let resp = ProblemDetail::new(StatusCode::BAD_REQUEST)
            .type_uri("https://example.com/problems/invalid-input")
            .title("Invalid Input")
            .detail("email is invalid")
            .instance("/users/42")
            .extension("request_id", "abc123")
            .into_response();

        let body = body_string(resp).await;
        assert!(body.contains("\"type\":\"https://example.com/problems/invalid-input\""));
        assert!(body.contains("\"title\":\"Invalid Input\""));
        assert!(body.contains("\"detail\":\"email is invalid\""));
        assert!(body.contains("\"instance\":\"/users/42\""));
        assert!(body.contains("\"request_id\":\"abc123\""));
    }

    #[tokio::test]
    async fn problem_detail_escapes_control_characters() {
        let resp = ProblemDetail::new(StatusCode::BAD_REQUEST)
            .detail("bad \"input\"\nnext line")
            .into_response();

        let body = body_string(resp).await;
        assert!(body.contains("\\\"input\\\""));
        assert!(body.contains("\\n"));
    }

    #[tokio::test]
    async fn problem_detail_ignores_reserved_extension_names() {
        let resp = ProblemDetail::new(StatusCode::BAD_REQUEST)
            .extension("status", "999")
            .extension("title", "shadow title")
            .extension("detail", "shadow detail")
            .extension("instance", "/shadow")
            .extension("type", "shadow:type")
            .extension("request_id", "abc123")
            .into_response();

        let body = body_string(resp).await;
        assert!(body.contains("\"status\":400"));
        assert!(body.contains("\"title\":\"Bad Request\""));
        assert!(!body.contains("shadow title"));
        assert!(!body.contains("shadow detail"));
        assert!(!body.contains("/shadow"));
        assert!(!body.contains("shadow:type"));
        assert!(body.contains("\"request_id\":\"abc123\""));
    }
}
