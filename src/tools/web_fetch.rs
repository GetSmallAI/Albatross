use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use super::Tool;

pub struct WebFetchTool {
    pub http: reqwest::Client,
}

#[derive(Deserialize)]
struct Args {
    url: String,
    #[serde(default = "default_max_bytes")]
    max_bytes: usize,
}

fn default_max_bytes() -> usize {
    65_536
}

const TIMEOUT_SECS: u64 = 15;

#[async_trait]
impl Tool for WebFetchTool {
    fn name(&self) -> &'static str {
        "web_fetch"
    }
    fn description(&self) -> &'static str {
        "Fetch a URL and return the response body as plain text (HTML tags stripped). Use for reading docs or RFCs the agent needs to consult."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "Absolute http(s) URL to fetch" },
                "max_bytes": {
                    "type": "integer",
                    "minimum": 1024,
                    "maximum": 524288,
                    "description": "Maximum response body length to return (default 64KB)"
                }
            },
            "required": ["url"]
        })
    }
    fn require_approval(&self, _args: &Value) -> bool {
        // Network egress with arbitrary URLs is "mutating" from a privacy
        // perspective — always gated.
        true
    }
    async fn execute(&self, args: Value) -> Value {
        let args: Args = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return json!({ "error": format!("invalid args: {e}") }),
        };
        if !args.url.starts_with("http://") && !args.url.starts_with("https://") {
            return json!({ "error": "url must start with http:// or https://" });
        }
        let resp = match self
            .http
            .get(&args.url)
            .header(
                "User-Agent",
                concat!("albatross/", env!("CARGO_PKG_VERSION")),
            )
            .timeout(std::time::Duration::from_secs(TIMEOUT_SECS))
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => return json!({ "error": format!("request failed: {e}") }),
        };
        let status = resp.status();
        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let body = match resp.text().await {
            Ok(b) => b,
            Err(e) => return json!({ "error": format!("read body failed: {e}") }),
        };
        let text = if content_type.contains("html") || body.trim_start().starts_with('<') {
            strip_html(&body)
        } else {
            body
        };
        let truncated = text.len() > args.max_bytes;
        let mut cutoff = args.max_bytes.min(text.len());
        while cutoff > 0 && !text.is_char_boundary(cutoff) {
            cutoff -= 1;
        }
        let body_out = if truncated {
            format!(
                "{}\n\n[… {} more bytes …]",
                &text[..cutoff],
                text.len() - cutoff
            )
        } else {
            text
        };
        json!({
            "status": status.as_u16(),
            "content_type": content_type,
            "truncated": truncated,
            "body": body_out,
        })
    }
}

/// Quick-and-dirty HTML-to-text: drop `<script>`/`<style>` blocks entirely,
/// strip every other tag, collapse whitespace runs, decode the most common
/// named entities. Not a real HTML parser — but small, dep-free, and
/// adequate for "give the model the readable text of a docs page."
fn strip_html(html: &str) -> String {
    let mut s = String::with_capacity(html.len());
    let bytes = html.as_bytes();
    let mut i = 0;
    let mut in_tag = false;
    let mut skip_until: Option<&[u8]> = None;
    while i < bytes.len() {
        if let Some(end) = skip_until {
            if lowercase_starts_with(&bytes[i..], end) {
                i += end.len();
                skip_until = None;
                continue;
            }
            i += 1;
            continue;
        }
        let b = bytes[i];
        if !in_tag && b == b'<' {
            if lowercase_starts_with(&bytes[i..], b"<script") {
                skip_until = Some(b"</script>");
                i += 1;
                continue;
            }
            if lowercase_starts_with(&bytes[i..], b"<style") {
                skip_until = Some(b"</style>");
                i += 1;
                continue;
            }
            in_tag = true;
            i += 1;
            continue;
        }
        if in_tag {
            if b == b'>' {
                in_tag = false;
                // Block-level tags become whitespace so paragraphs don't smush.
                s.push(' ');
            }
            i += 1;
            continue;
        }
        s.push(b as char);
        i += 1;
    }
    let decoded = decode_entities(&s);
    collapse_whitespace(&decoded)
}

fn lowercase_starts_with(haystack: &[u8], needle_lc: &[u8]) -> bool {
    haystack.len() >= needle_lc.len()
        && haystack[..needle_lc.len()]
            .iter()
            .zip(needle_lc.iter())
            .all(|(a, b)| a.to_ascii_lowercase() == *b)
}

fn decode_entities(s: &str) -> String {
    s.replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
}

fn collapse_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_was_ws = true; // trims leading whitespace
    for c in s.chars() {
        if c.is_whitespace() {
            if !last_was_ws {
                out.push(if c == '\n' { '\n' } else { ' ' });
                last_was_ws = true;
            } else if c == '\n' && !out.ends_with('\n') {
                // Preserve paragraph breaks even after intervening spaces.
                if out.ends_with(' ') {
                    out.pop();
                }
                out.push('\n');
            }
        } else {
            out.push(c);
            last_was_ws = false;
        }
    }
    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_html_tags_and_decodes_basic_entities() {
        let html = "<html><head><title>x</title></head><body><p>Hello &amp; <b>world</b></p></body></html>";
        let text = strip_html(html);
        assert!(text.contains("Hello & world"));
        assert!(!text.contains('<'));
        assert!(!text.contains('>'));
    }

    #[test]
    fn drops_script_and_style_contents() {
        let html = "<style>body{color:red}</style><script>alert('hi')</script><p>visible</p>";
        let text = strip_html(html);
        assert!(text.contains("visible"));
        assert!(!text.contains("alert"));
        assert!(!text.contains("color:red"));
    }

    #[test]
    fn collapses_excess_whitespace() {
        let html = "<p>one    two\n\n\nthree</p>";
        let text = strip_html(html);
        // Multiple spaces collapsed; paragraph break preserved at most as
        // a single newline.
        assert!(!text.contains("    "));
        assert!(text.contains("one two"));
        assert!(text.contains("three"));
    }

    #[test]
    fn handles_attribute_with_angle_bracket_lookalike_in_text() {
        let html = "<p>2 &lt; 3</p>";
        assert!(strip_html(html).contains("2 < 3"));
    }

    #[test]
    fn case_insensitive_script_match() {
        let html = "<SCRIPT>bad</SCRIPT><p>good</p>";
        let text = strip_html(html);
        assert!(text.contains("good"));
        assert!(!text.contains("bad"));
    }
}
