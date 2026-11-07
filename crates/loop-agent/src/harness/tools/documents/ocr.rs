//! OCR for PDFs without a usable text layer, through a Mistral-compatible
//! OCR API. Off unless `LOOP_PDF_OCR_API_KEY` is set; `LOOP_PDF_OCR_URL` and
//! `LOOP_PDF_OCR_MODEL` point it at another endpoint or model.

pub(super) const KEY_ENV: &str = "LOOP_PDF_OCR_API_KEY";
#[cfg(feature = "web")]
const URL_ENV: &str = "LOOP_PDF_OCR_URL";
#[cfg(feature = "web")]
const MODEL_ENV: &str = "LOOP_PDF_OCR_MODEL";
#[cfg(feature = "web")]
const DEFAULT_URL: &str = "https://api.mistral.ai/v1/ocr";
#[cfg(feature = "web")]
const DEFAULT_MODEL: &str = "mistral-ocr-latest";

fn env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
}

/// Whether OCR is configured (and built in).
pub(super) fn enabled() -> bool {
    cfg!(feature = "web") && env(KEY_ENV).is_some()
}

/// The text of `pdf` as markdown, one section per page.
#[cfg(feature = "web")]
pub(super) async fn run(pdf: &[u8]) -> Result<String, String> {
    use base64::Engine as _;
    use std::time::Duration;

    let key = env(KEY_ENV).ok_or_else(|| format!("{KEY_ENV} is not set"))?;
    let url = env(URL_ENV).unwrap_or_else(|| DEFAULT_URL.to_owned());
    let model = env(MODEL_ENV).unwrap_or_else(|| DEFAULT_MODEL.to_owned());
    let data = base64::engine::general_purpose::STANDARD.encode(pdf);
    let body = serde_json::json!({
        "model": model,
        "document": {
            "type": "document_url",
            "document_url": format!("data:application/pdf;base64,{data}"),
        },
    });
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(300))
        .build()
        .map_err(|e| e.to_string())?;
    let response = client
        .post(&url)
        .bearer_auth(key)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("OCR request failed: {e}"))?;
    let status = response.status();
    if !status.is_success() {
        let detail = response.text().await.unwrap_or_default();
        let detail: String = detail.chars().take(300).collect();
        return Err(format!("OCR service returned {status}: {detail}"));
    }
    let value: serde_json::Value = response
        .json()
        .await
        .map_err(|e| format!("OCR response was not JSON: {e}"))?;
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

#[cfg(not(feature = "web"))]
pub(super) async fn run(_pdf: &[u8]) -> Result<String, String> {
    Err("OCR needs loop-agent's `web` feature".into())
}
