//! One readable line for a failed HTTP response.
//!
//! Provider errors arrive as JSON, plain text or whole HTML error pages (e.g. an nginx
//! 502). Every place that reports an HTTP failure uses [`summarize_http_error`], so users
//! see `HTTP 502 Bad Gateway` or `HTTP 401 Unauthorized: Invalid API key` instead of markup.

/// Longest body excerpt kept after the status line.
const MAX_DETAIL_CHARS: usize = 200;

/// `HTTP <status> <reason>[: <detail>]`, where the detail is the provider's error message
/// (JSON), the page title (HTML) or the trimmed text, and omitted when it adds nothing.
pub fn summarize_http_error(status: u16, body: &str) -> String {
    let reason = reqwest::StatusCode::from_u16(status)
        .ok()
        .and_then(|s| s.canonical_reason());
    let head = match reason {
        Some(reason) => format!("HTTP {status} {reason}"),
        None => format!("HTTP {status}"),
    };
    match body_detail(body) {
        Some(detail) if !is_redundant(&detail, status, reason) => format!("{head}: {detail}"),
        _ => head,
    }
}

fn body_detail(body: &str) -> Option<String> {
    let body = body.trim();
    if body.is_empty() {
        return None;
    }
    let detail = if let Ok(json) = serde_json::from_str::<serde_json::Value>(body) {
        json_message(&json).unwrap_or_else(|| body.to_string())
    } else if looks_like_html(body) {
        html_title(body).unwrap_or_else(|| strip_tags(body))
    } else {
        body.to_string()
    };
    let detail = collapse_whitespace(&detail);
    (!detail.is_empty()).then(|| truncate(&detail, MAX_DETAIL_CHARS))
}

/// `{"error":{"message":…}}`, `{"error":"…"}`, `{"message":…}` or `{"detail":…}`.
fn json_message(json: &serde_json::Value) -> Option<String> {
    let text = |v: &serde_json::Value| v.as_str().map(str::to_string);
    json.get("error")
        .and_then(|e| e.get("message").and_then(text).or_else(|| text(e)))
        .or_else(|| json.get("message").and_then(text))
        .or_else(|| json.get("detail").and_then(text))
}

fn looks_like_html(body: &str) -> bool {
    let start = body.trim_start().to_ascii_lowercase();
    start.starts_with("<!doctype html") || start.starts_with("<html") || start.starts_with("<head")
}

fn html_title(body: &str) -> Option<String> {
    let lower = body.to_ascii_lowercase();
    let start = lower.find("<title>")? + "<title>".len();
    let end = start + lower[start..].find("</title>")?;
    Some(body[start..end].to_string())
}

fn strip_tags(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    let mut in_tag = false;
    for c in body.chars() {
        match c {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                out.push(' ');
            }
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate(text: &str, max_chars: usize) -> String {
    match text.char_indices().nth(max_chars) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text.to_string(),
    }
}

/// A detail that only repeats the status line (nginx's `<title>502 Bad Gateway</title>`).
fn is_redundant(detail: &str, status: u16, reason: Option<&str>) -> bool {
    let detail = detail.to_ascii_lowercase();
    let status_line = match reason {
        Some(reason) => format!("{status} {}", reason.to_ascii_lowercase()),
        None => status.to_string(),
    };
    detail == status_line || detail == status.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const NGINX_502: &str = "<html>\r\n<head><title>502 Bad Gateway</title></head>\r\n<body>\r\n\
        <center><h1>502 Bad Gateway</h1></center>\r\n<hr><center>nginx/1.18.0 (Ubuntu)</center>\r\n\
        </body>\r\n</html>\r\n";

    #[test]
    fn nginx_html_page_becomes_the_status_line() {
        assert_eq!(summarize_http_error(502, NGINX_502), "HTTP 502 Bad Gateway");
    }

    #[test]
    fn html_title_is_kept_when_it_says_something_new() {
        let body = "<!DOCTYPE html><html><head><title>Maintenance window</title></head></html>";
        assert_eq!(
            summarize_http_error(503, body),
            "HTTP 503 Service Unavailable: Maintenance window"
        );
    }

    #[test]
    fn json_error_shapes() {
        assert_eq!(
            summarize_http_error(401, r#"{"error":{"message":"Invalid API key","code":401}}"#),
            "HTTP 401 Unauthorized: Invalid API key"
        );
        assert_eq!(
            summarize_http_error(400, r#"{"error":"model not found"}"#),
            "HTTP 400 Bad Request: model not found"
        );
        assert_eq!(
            summarize_http_error(422, r#"{"detail":"bad field"}"#),
            "HTTP 422 Unprocessable Entity: bad field"
        );
        assert_eq!(
            summarize_http_error(500, r#"{"unexpected":true}"#),
            r#"HTTP 500 Internal Server Error: {"unexpected":true}"#
        );
    }

    #[test]
    fn plain_text_and_empty_bodies() {
        assert_eq!(
            summarize_http_error(429, "  rate   limited\n try later "),
            "HTTP 429 Too Many Requests: rate limited try later"
        );
        assert_eq!(
            summarize_http_error(500, "   "),
            "HTTP 500 Internal Server Error"
        );
        assert_eq!(summarize_http_error(599, ""), "HTTP 599");
    }

    #[test]
    fn long_bodies_are_truncated_on_a_char_boundary() {
        let body = "é".repeat(500);
        let summary = summarize_http_error(500, &body);
        let detail = summary.split_once(": ").unwrap().1;
        assert_eq!(detail.chars().count(), MAX_DETAIL_CHARS + 1);
        assert!(detail.ends_with('…'));
    }
}
