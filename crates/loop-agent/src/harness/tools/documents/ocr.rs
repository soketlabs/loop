//! OCR for PDFs without a usable text layer, through a Mistral-compatible
//! OCR API. Off unless `LOOP_PDF_OCR_API_KEY` is set; `LOOP_PDF_OCR_URL` and
//! `LOOP_PDF_OCR_MODEL` point it at another endpoint or model.
//!
//! OCR uploads the whole PDF to that endpoint from the Loop process, not
//! from a Session's sandbox, and web permissions do not apply to it. So it
//! only runs over `https`, for unencrypted PDFs of at most
//! [`MAX_UPLOAD_BYTES`], and only after the reader found no text layer.

use super::child::PdfInfo;

pub(super) const KEY_ENV: &str = "LOOP_PDF_OCR_API_KEY";
const URL_ENV: &str = "LOOP_PDF_OCR_URL";
const MODEL_ENV: &str = "LOOP_PDF_OCR_MODEL";
const DEFAULT_URL: &str = "https://api.mistral.ai/v1/ocr";
const DEFAULT_MODEL: &str = "mistral-ocr-latest";

/// The largest PDF sent to OCR.
pub(super) const MAX_UPLOAD_BYTES: usize = 20 * 1024 * 1024;
/// The largest OCR response read.
#[cfg_attr(not(feature = "web"), allow(dead_code))]
const MAX_RESPONSE_BYTES: usize = 32 * 1024 * 1024;

/// Where and how to run OCR.
#[derive(Debug, Clone)]
#[cfg_attr(not(feature = "web"), allow(dead_code))]
pub(super) struct Config {
    pub key: String,
    pub url: String,
    pub model: String,
}

fn env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
}

impl Config {
    /// The configuration from the environment, when OCR is on (and built in).
    /// Off behind an API proxy ([`loop_ai::API_PROXY_SOCKET_ENV`]): OCR would
    /// otherwise go straight to its endpoint with a key held by Loop.
    pub(super) fn from_env() -> Option<Self> {
        if !cfg!(feature = "web") || loop_ai::api_proxy_socket().is_some() {
            return None;
        }
        Some(Self {
            key: env(KEY_ENV)?,
            url: env(URL_ENV).unwrap_or_else(|| DEFAULT_URL.to_owned()),
            model: env(MODEL_ENV).unwrap_or_else(|| DEFAULT_MODEL.to_owned()),
        })
    }

    /// The endpoint must be `https`: the request carries the key and the
    /// whole document.
    pub(super) fn check(&self) -> Result<(), String> {
        let scheme = self
            .url
            .split_once("://")
            .map(|(s, _)| s.to_ascii_lowercase());
        if scheme.as_deref() != Some("https") {
            return Err(format!("{URL_ENV} must be an https:// URL"));
        }
        Ok(())
    }
}

/// Whether the PDF in `bytes`, about which the reader said `info`, may be
/// sent to OCR. The bytes are also searched for an encryption dictionary,
/// since a reader that failed to load the file cannot tell.
pub(super) fn permitted(info: Option<PdfInfo>, bytes: &[u8]) -> Result<(), String> {
    let encrypted = info.is_some_and(|i| i.encrypted) || bytes.windows(8).any(|w| w == b"/Encrypt");
    if encrypted {
        return Err("the PDF is encrypted, so it is not sent to OCR".into());
    }
    if bytes.len() > MAX_UPLOAD_BYTES {
        return Err(format!(
            "the PDF is larger than the {} MB OCR upload limit",
            MAX_UPLOAD_BYTES / (1024 * 1024)
        ));
    }
    Ok(())
}

/// The text of `pdf` as markdown, one section per page.
#[cfg(feature = "web")]
pub(super) async fn run(config: &Config, pdf: &[u8]) -> Result<String, String> {
    use base64::Engine as _;
    use std::time::Duration;

    config.check()?;
    permitted(Some(PdfInfo::default()), pdf)?;
    let data = base64::engine::general_purpose::STANDARD.encode(pdf);
    let body = serde_json::json!({
        "model": config.model,
        "document": {
            "type": "document_url",
            "document_url": format!("data:application/pdf;base64,{data}"),
        },
    });
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(300))
        .https_only(true)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| e.to_string())?;
    let mut response = client
        .post(&config.url)
        .bearer_auth(&config.key)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("OCR request failed: {e}"))?;
    let status = response.status();
    let mut raw = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| format!("OCR response failed: {e}"))?
    {
        if raw.len() + chunk.len() > MAX_RESPONSE_BYTES {
            return Err(format!(
                "the OCR response is larger than {} MB",
                MAX_RESPONSE_BYTES / (1024 * 1024)
            ));
        }
        raw.extend_from_slice(&chunk);
    }
    if !status.is_success() {
        let detail: String = String::from_utf8_lossy(&raw).chars().take(300).collect();
        return Err(format!("OCR service returned {status}: {detail}"));
    }
    let value: serde_json::Value =
        serde_json::from_slice(&raw).map_err(|e| format!("OCR response was not JSON: {e}"))?;
    text_of(&value)
}

#[cfg(not(feature = "web"))]
pub(super) async fn run(config: &Config, _pdf: &[u8]) -> Result<String, String> {
    config.check()?;
    Err("OCR needs loop-agent's `web` feature".into())
}

/// The markdown in an OCR response.
#[cfg_attr(not(feature = "web"), allow(dead_code))]
fn text_of(value: &serde_json::Value) -> Result<String, String> {
    let pages: Vec<&str> = value["pages"]
        .as_array()
        .map(|pages| {
            pages
                .iter()
                .filter_map(|p| p["markdown"].as_str())
                .collect()
        })
        .unwrap_or_default();
    let text = match pages.as_slice() {
        [] => value["markdown"]
            .as_str()
            .or(value["text"].as_str())
            .unwrap_or_default()
            .to_owned(),
        [only] => only.to_string(),
        many => many
            .iter()
            .enumerate()
            .map(|(i, page)| format!("<!-- page {} -->\n\n{}", i + 1, page.trim()))
            .collect::<Vec<_>>()
            .join("\n\n"),
    };
    if text.trim().is_empty() {
        return Err("the OCR service found no text".into());
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(url: &str) -> Config {
        Config {
            key: "k".into(),
            url: url.into(),
            model: "m".into(),
        }
    }

    #[test]
    fn only_https_endpoints_are_used() {
        assert!(config("https://ocr.example/v1").check().is_ok());
        assert!(config("HTTPS://ocr.example/v1").check().is_ok());
        for url in ["http://ocr.example/v1", "ocr.example", "file:///etc/passwd"] {
            assert!(config(url).check().unwrap_err().contains("https"), "{url}");
        }
    }

    #[tokio::test]
    async fn an_http_endpoint_fails_before_any_request() {
        // 192.0.2.0/24 is TEST-NET: never routed. The check fails first.
        let err = run(&config("http://192.0.2.1/ocr"), b"%PDF-1.4")
            .await
            .unwrap_err();
        assert!(err.contains("https") || err.contains("web"), "{err}");
    }

    #[test]
    fn encrypted_or_large_pdfs_are_not_sent() {
        let plain = PdfInfo::default();
        assert!(permitted(Some(plain), b"%PDF").is_ok());
        let encrypted = PdfInfo {
            encrypted: true,
            readable: false,
        };
        assert!(permitted(Some(encrypted), b"%PDF")
            .unwrap_err()
            .contains("encrypted"));
        // The reader could not tell: the bytes decide.
        assert!(permitted(None, b"%PDF trailer << /Encrypt 5 0 R >>")
            .unwrap_err()
            .contains("encrypted"));
        assert!(permitted(None, b"%PDF").is_ok());
        let big = vec![b' '; MAX_UPLOAD_BYTES + 1];
        assert!(permitted(Some(plain), &big).unwrap_err().contains("limit"));
    }

    #[test]
    fn responses_become_page_sections() {
        let v = serde_json::json!({"pages": [{"markdown": "a"}, {"markdown": "b"}]});
        assert_eq!(
            text_of(&v).unwrap(),
            "<!-- page 1 -->\n\na\n\n<!-- page 2 -->\n\nb"
        );
        assert!(text_of(&serde_json::json!({})).is_err());
    }
}
