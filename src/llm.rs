//! OpenAI chat completion client for the plan summary.
//!
//! Blocking reqwest call (v0.0.1). Streaming deferred to v0.0.2.
//! The system prompt instructs the model to surface risky changes with
//! a `Risky:` prefix — that's the product signal.

use serde::{Deserialize, Serialize};
use std::time::Duration;

const OPENAI_URL: &str = "https://api.openai.com/v1/chat/completions";
const REQUEST_TIMEOUT_SECS: u64 = 30;

const SYSTEM_PROMPT: &str = "You are a terraform plan reviewer. The first line of \
the user message is the authoritative plan footer (e.g. 'Plan: 2 to add, 1 to change, \
0 to destroy'). NEVER contradict these numbers or invent your own counts. Summarize \
the plan in 2-3 sentences, quoting the footer numbers exactly. Call out risky changes \
with a 'Risky:' prefix on a separate clause. Risky = destroys, replaces of stateful \
resources, security_group ingress opened to 0.0.0.0/0, IAM wildcards, RDS \
deletion_protection disabled, EBS unencrypted. If nothing is risky, say so plainly \
instead of inventing risks. Be terse.";

/// Outcome of the LLM call, ready to render in the summary pane.
#[derive(Debug, Clone)]
pub enum Summary {
    Ok(String),
    Disabled(&'static str),
    Error(String),
}

impl Summary {
    pub fn text(&self) -> &str {
        match self {
            Summary::Ok(s) => s,
            Summary::Disabled(s) => s,
            Summary::Error(s) => s,
        }
    }
}

pub fn disabled_reason(no_ai_flag: bool) -> &'static str {
    if no_ai_flag {
        "AI summary disabled (--no-ai)."
    } else {
        "AI summary disabled: OPENAI_API_KEY not set. Set the key or pass --no-ai."
    }
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<Message<'a>>,
    temperature: f32,
}

#[derive(Serialize)]
struct Message<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    message: MessageOwned,
}

#[derive(Deserialize)]
struct MessageOwned {
    #[allow(dead_code)]
    role: String,
    content: String,
}

/// Blocking fetch. Returns a [`Summary`] variant so callers always have something to render.
pub fn fetch_summary(digest: &str, model: &str) -> Summary {
    let api_key = match std::env::var("OPENAI_API_KEY") {
        Ok(k) if !k.is_empty() => k,
        _ => return Summary::Disabled(disabled_reason(false)),
    };

    let client = match reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
        .build()
    {
        Ok(c) => c,
        Err(e) => return Summary::Error(format!("http client init failed: {e}")),
    };

    let body = ChatRequest {
        model,
        messages: vec![
            Message {
                role: "system",
                content: SYSTEM_PROMPT,
            },
            Message {
                role: "user",
                content: digest,
            },
        ],
        temperature: 0.2,
    };

    let response = client
        .post(OPENAI_URL)
        .bearer_auth(api_key)
        .json(&body)
        .send();

    let response = match response {
        Ok(r) => r,
        Err(e) if e.is_timeout() => {
            return Summary::Error("AI summary timed out after 30s.".into());
        }
        Err(e) => return Summary::Error(format!("AI summary request failed: {e}")),
    };

    let status = response.status();
    if !status.is_success() {
        let body_text = response.text().unwrap_or_default();
        let snippet = body_text.chars().take(200).collect::<String>();
        let hint = match status.as_u16() {
            401 | 403 => " (check OPENAI_API_KEY)",
            429 => " (rate limited — try again in a moment)",
            500..=599 => " (OpenAI server error)",
            _ => "",
        };
        return Summary::Error(format!(
            "AI summary returned HTTP {status}{hint}: {snippet}"
        ));
    }

    let parsed: Result<ChatResponse, _> = response.json();
    match parsed {
        Ok(r) => match r.choices.into_iter().next() {
            Some(choice) => Summary::Ok(choice.message.content.trim().to_string()),
            None => Summary::Error("AI summary returned no choices.".into()),
        },
        Err(e) => Summary::Error(format!("AI summary response parse failed: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_reason_with_flag() {
        assert!(disabled_reason(true).contains("--no-ai"));
    }

    #[test]
    fn disabled_reason_without_flag() {
        assert!(disabled_reason(false).contains("OPENAI_API_KEY"));
    }

    #[test]
    fn summary_text_accessor() {
        assert_eq!(Summary::Ok("ok".into()).text(), "ok");
        assert_eq!(Summary::Disabled("disabled").text(), "disabled");
        assert_eq!(Summary::Error("err".into()).text(), "err");
    }
}
