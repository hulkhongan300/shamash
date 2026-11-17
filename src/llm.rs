//! Optional LLM stage that guesses the real names in a misheard transcript
//! ("hoolig bts" -> "Hooligan by BTS"), which the catalog cannot rescue.
//! Off by default (costs a round trip); set `LLM_ENDPOINT` to enable.

use anyhow::Context as _;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// A stalled rewrite falls back to the transcript instead of blocking the command.
const REWRITE_TIMEOUT: Duration = Duration::from_secs(3);

/// Longer replies are commentary, not a search query.
const MAX_REPLY_CHARS: usize = 160;

const MAX_REPLY_TOKENS: usize = 64;

const INSTRUCTION: &str = "\
A speech recogniser heard someone ask a voice assistant for a song. It often \
mishears the song title and the artist's name, and the assistant may also have \
heard its own wake word at the start.

Reply with the most likely real song request, formatted as exactly \
\"<title> by <artist>\" on a single line. Nothing else: no quotes, no \
explanation, no preamble, no trailing punctuation. Keep the words the speaker \
actually said if they are already a plausible song request. If you cannot \
guess a name, leave it as it was heard.";

/// Ollama-compatible chat request body.
#[derive(Debug, Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<Message<'a>>,
    /// The reply is only useful whole.
    stream: bool,
    options: Options,
}

#[derive(Debug, Serialize)]
struct Message<'a> {
    role: &'a str,
    content: String,
}

/// Pinned so the same transcript gives the same request.
#[derive(Debug, Serialize)]
struct Options {
    temperature: f32,
    num_predict: usize,
}

#[derive(Debug, Deserialize)]
struct ChatReply {
    message: Reply,
}

#[derive(Debug, Deserialize)]
struct Reply {
    content: String,
}

/// Asks the endpoint to clean up `transcript`; `Ok(None)` means no better answer.
pub async fn rewrite(
    http: &reqwest::Client,
    endpoint: &str,
    model: &str,
    transcript: &str,
) -> anyhow::Result<Option<String>> {
    let transcript = transcript.trim();
    if transcript.is_empty() {
        return Ok(None);
    }

    let url = format!("{}/api/chat", endpoint.trim_end_matches('/'));
    let body = ChatRequest {
        model,
        messages: vec![Message {
            role: "user",
            content: format!("{INSTRUCTION}\n\nHeard: {transcript}"),
        }],
        stream: false,
        options: Options {
            temperature: 0.0,
            num_predict: MAX_REPLY_TOKENS,
        },
    };

    let response = http
        .post(&url)
        .timeout(REWRITE_TIMEOUT)
        .json(&body)
        .send()
        .await
        .with_context(|| format!("could not reach the rewrite endpoint at {url}"))?;
    // Before decoding, so an error page reports its status, not a decode failure.
    let status = response.status();
    anyhow::ensure!(
        status.is_success(),
        "the rewrite endpoint at {url} answered HTTP {status}"
    );
    let reply: ChatReply = response
        .json()
        .await
        .context("the rewrite endpoint did not answer with a chat reply")?;

    Ok(clean(&reply.message.content))
}

/// Reduces a reply to `title by artist`; several unlabelled lines are a hedge and refused.
fn clean(reply: &str) -> Option<String> {
    let lines: Vec<String> = reply
        .lines()
        .map(|line| unquote(line.trim()))
        .filter(|line| !line.is_empty())
        .collect();
    if lines.is_empty() {
        return None;
    }

    let mut title = None;
    let mut artist = None;
    let mut unlabelled = Vec::new();
    for line in &lines {
        match label_of(line) {
            Some("title") => title = Some(value_after_label(line)),
            Some("artist") => artist = Some(value_after_label(line)),
            _ => unlabelled.push(line.clone()),
        }
    }
    let title = title.filter(|value| !value.is_empty());
    let artist = artist.filter(|value| !value.is_empty());

    let joined = match (title, artist) {
        (Some(title), Some(artist)) => format!("{title} by {artist}"),
        (Some(title), None) => title,
        (None, Some(artist)) => artist,
        (None, None) if unlabelled.len() == 1 => unlabelled.remove(0),
        (None, None) => return None,
    };

    // Models often use a dash for "by"; only en/em dashes, since hyphens are in names like "Jay-Z".
    let joined = joined.replace(['\u{2014}', '\u{2013}'], " by ");
    let joined = unquote(joined.trim());
    let joined = joined.split_whitespace().collect::<Vec<_>>().join(" ");
    if joined.is_empty() || joined.len() > MAX_REPLY_CHARS {
        return None;
    }
    Some(joined)
}

fn label_of(line: &str) -> Option<&'static str> {
    let lowered = line.to_ascii_lowercase();
    ["title", "artist"]
        .into_iter()
        .find(|label| lowered.starts_with(label))
}

fn value_after_label(line: &str) -> String {
    line.split_once(':')
        .map(|(_, value)| value.trim().to_string())
        .unwrap_or_default()
}

/// Strips one layer of quotes and trailing punctuation, but not a full stop ("Run-D.M.C.").
fn unquote(text: &str) -> String {
    let trimmed = text.trim().trim_end_matches(['!', ',', ';']);
    let trimmed = trimmed.trim();
    for quote in ['"', '\'', '\u{201c}', '\u{201d}'] {
        if let Some(inner) = trimmed
            .strip_prefix(quote)
            .and_then(|t| t.strip_suffix(quote))
        {
            return inner.trim().to_string();
        }
    }
    trimmed.to_string()
}

#[cfg(test)]
#[path = "tests/llm.rs"]
mod tests;
