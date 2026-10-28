//! The language-model call: one blocking OpenAI-compatible chat completion.
//!
//! Why a healthy-looking endpoint can return nothing, and why `reasoning_effort` is sent
//! unconditionally: `docs/ai-summary.md` in agentmux, "LM backend: why a healthy-looking
//! endpoint returns nothing". Every failure is `None`; the summaries are cosmetic and the
//! caller keeps its last good value.

use serde_json::{json, Value};
use std::time::Duration;

/// A text-completion backend.
pub trait Model: Send + Sync {
    /// The model's reply to `system` + `user`, or `None` on any failure or empty reply.
    fn complete(&self, system: &str, user: &str) -> Option<String>;
}

/// An OpenAI-compatible `/v1/chat/completions` endpoint (LM Studio, Ollama, ...).
#[derive(Debug, Clone)]
pub struct HttpModel {
    /// The full completions URL, posted to as given.
    pub url: String,
    /// The model id sent in the request body.
    pub model: String,
    /// Whole-request deadline; a slower model is `None`, not a hang.
    pub timeout: Duration,
}

impl HttpModel {
    /// The request body. `reasoning_effort: "none"` is load-bearing: a hybrid-reasoning model
    /// left to think spends `max_tokens` on reasoning and returns empty content. Non-reasoning
    /// models ignore the field.
    fn body(&self, system: &str, user: &str) -> Value {
        json!({
            "model": self.model,
            "temperature": 0,
            "max_tokens": 200,
            "stream": false,
            "reasoning_effort": "none",
            "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": user},
            ],
        })
    }
}

impl Model for HttpModel {
    fn complete(&self, system: &str, user: &str) -> Option<String> {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(self.timeout))
            .http_status_as_error(false)
            .build()
            .into();
        let mut resp = agent
            .post(&self.url)
            .send_json(self.body(system, user))
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let text = strip_control_bytes(&resp.body_mut().read_to_string().ok()?);
        content_of(&serde_json::from_str::<Value>(&text).ok()?)
    }
}

/// The body without raw control bytes (`\x00-\x08`, `\x0b-\x1f`, `\x7f`; tab and newline stay).
/// A non-conformant backend that emits them unescaped inside a JSON string would otherwise fail
/// the parse and silence the summary for good.
fn strip_control_bytes(s: &str) -> String {
    s.chars()
        .filter(|c| !matches!(*c, '\x00'..='\x08' | '\x0b'..='\x1f' | '\x7f'))
        .collect()
}

/// The reply text of a parsed completion; `None` for an `.error` body, absent or non-string
/// content, or empty content (including a `finish_reason` of `length` that left it empty).
fn content_of(v: &Value) -> Option<String> {
    if v.get("error").is_some_and(|e| !e.is_null()) {
        return None;
    }
    let text = v.pointer("/choices/0/message/content")?.as_str()?.trim();
    (!text.is_empty()).then(|| text.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::thread;

    /// One-shot HTTP/1.1 responder on a loopback port. Returns the URL and a channel that
    /// yields the request body. `delay` holds the reply back.
    fn fake(status: &str, reply: &str, delay: Duration) -> (String, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!(
            "http://{}/v1/chat/completions",
            listener.local_addr().unwrap()
        );
        let (tx, rx) = mpsc::channel();
        let (status, reply) = (status.to_string(), reply.to_string());
        thread::spawn(move || {
            let Ok((mut s, _)) = listener.accept() else {
                return;
            };
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            let body = loop {
                let n = s.read(&mut chunk).unwrap_or(0);
                buf.extend_from_slice(&chunk[..n]);
                let text = String::from_utf8_lossy(&buf).to_string();
                if let Some(i) = text.find("\r\n\r\n") {
                    let len = text[..i]
                        .lines()
                        .find_map(|l| {
                            let l = l.to_ascii_lowercase();
                            l.strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                        })
                        .unwrap_or(0);
                    if text.len() >= i + 4 + len {
                        break text[i + 4..i + 4 + len].to_string();
                    }
                }
                if n == 0 {
                    break text;
                }
            };
            let _ = tx.send(body);
            thread::sleep(delay);
            let _ = write!(
                s,
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                reply.len()
            );
        });
        (url, rx)
    }

    fn model(url: String, timeout: Duration) -> HttpModel {
        HttpModel {
            url,
            model: "m1".into(),
            timeout,
        }
    }

    const OK: &str =
        r#"{"choices":[{"message":{"content":"secret label"},"finish_reason":"stop"}]}"#;

    #[test]
    fn http_model_sends_reasoning_effort_none() {
        let (url, rx) = fake("200 OK", OK, Duration::ZERO);
        let got = model(url, Duration::from_secs(5)).complete("SYS", "SECRETDIGEST words");
        assert_eq!(got.as_deref(), Some("secret label"));
        let body: Value = serde_json::from_str(&rx.recv().unwrap()).unwrap();
        assert_eq!(body["reasoning_effort"], "none");
        assert_eq!(body["model"], "m1");
        assert_eq!(body["temperature"], 0);
        assert_eq!(body["max_tokens"], 200);
        assert_eq!(body["stream"], false);
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][0]["content"], "SYS");
        assert_eq!(body["messages"][1]["role"], "user");
        assert_eq!(body["messages"][1]["content"], "SECRETDIGEST words");
    }

    #[test]
    fn http_model_length_with_empty_content_is_none() {
        let r = r#"{"choices":[{"message":{"content":""},"finish_reason":"length"}]}"#;
        let (url, _rx) = fake("200 OK", r, Duration::ZERO);
        assert_eq!(model(url, Duration::from_secs(5)).complete("s", "u"), None);
    }

    #[test]
    fn http_model_timeout_is_none() {
        let (url, _rx) = fake("200 OK", OK, Duration::from_secs(3));
        assert_eq!(
            model(url, Duration::from_millis(300)).complete("s", "u"),
            None
        );
    }

    #[test]
    fn http_model_tolerates_raw_control_bytes() {
        let r = "{\"choices\":[{\"message\":{\"content\":\"sec\u{1}ret\u{7f} label\"}}]}";
        let (url, _rx) = fake("200 OK", r, Duration::ZERO);
        let got = model(url, Duration::from_secs(5)).complete("s", "u");
        assert_eq!(got.as_deref(), Some("secret label"));
    }

    #[test]
    fn http_model_non_2xx_is_none() {
        let (url, _rx) = fake("500 Internal Server Error", OK, Duration::ZERO);
        assert_eq!(model(url, Duration::from_secs(5)).complete("s", "u"), None);
    }

    #[test]
    fn http_model_error_body_is_none() {
        let (url, _rx) = fake(
            "200 OK",
            r#"{"error":{"message":"no model"}}"#,
            Duration::ZERO,
        );
        assert_eq!(model(url, Duration::from_secs(5)).complete("s", "u"), None);
    }

    #[test]
    fn http_model_unreachable_is_none() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/", l.local_addr().unwrap());
        drop(l);
        assert_eq!(model(url, Duration::from_secs(2)).complete("s", "u"), None);
    }
}
