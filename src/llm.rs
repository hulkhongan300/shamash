//! An optional stage that turns a misheard transcript into a cleaner one.
//!
//! Speech-to-text renders a title and an artist as whatever letters it hears --
//! "hoolig bts" for "Hooligan, BTS" -- and everything downstream then works from
//! those letters. A small instruction model is asked to guess the real names,
//! which is a task it is good at and a catalog lookup is not: the catalog can
//! only match what it is given, so a title mangled past recognition can never be
//! found no matter how good the matching is.
//!
//! It is deliberately not the default. The catalog already corrects most
//! mishearing for free and returns the track's real running time, so this stage
//! has to earn its place on the requests the catalog cannot rescue, and it costs
//! a model load and a round trip on every one of them. Set `LLM_ENDPOINT` to
//! switch it on.

use anyhow::Context as _;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// How long the rewrite may take before the transcript is used as heard.
///
/// The catalog lookup it feeds is already bounded at two seconds and the search
/// after it is slower still, so a rewrite that stalls must not become the reason
/// a command goes unanswered. The timeout is what makes this stage safe to leave
/// enabled: every failure falls back rather than blocking.
const REWRITE_TIMEOUT: Duration = Duration::from_secs(3);

/// The whole reply has to be one line, because it is read as a search query and
/// a model that explains itself produces a query nobody asked for.
const MAX_REPLY_CHARS: usize = 160;

/// Generation cap, in tokens. Small because the format is one line.
const MAX_REPLY_TOKENS: usize = 64;

/// What the model is told to do with the transcript.
const INSTRUCTION: &str = "\
A speech recogniser heard someone ask a voice assistant for a song. It often \
mishears the song title and the artist's name, and the assistant may also have \
heard its own wake word at the start.

Reply with the most likely real song request, formatted as exactly \
\"<title> by <artist>\" on a single line. Nothing else: no quotes, no \
explanation, no preamble, no trailing punctuation. Keep the words the speaker \
actually said if they are already a plausible song request. If you cannot \
guess a name, leave it as it was heard.";

/// The Ollama-compatible chat request body.
#[derive(Debug, Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<Message<'a>>,
    /// One complete response, not a token stream: the reply is only useful
    /// whole, and streaming it would mean holding the pipeline open for nothing.
    stream: bool,
    options: Options,
}

/// One turn of the conversation.
#[derive(Debug, Serialize)]
struct Message<'a> {
    role: &'a str,
    content: String,
}

/// Generation settings, pinned so the same transcript gives the same request.
#[derive(Debug, Serialize)]
struct Options {
    temperature: f32,
    /// One line of output is the format, not a preference.
    num_predict: usize,
}

/// The reply, as Ollama returns it.
#[derive(Debug, Deserialize)]
struct ChatReply {
    message: Reply,
}

#[derive(Debug, Deserialize)]
struct Reply {
    content: String,
}

/// Asks the endpoint to clean up `transcript`.
///
/// `Ok(None)` means "no better answer than the transcript", for every reason
/// worth not failing a command over: the model declined, the reply was not a
/// song request, or the endpoint was unreachable. The caller plays what was
/// heard in all of them.
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
    // Checked before decoding: an error page, a proxy's HTML, or a 500 with an
    // empty body all decode-fail with a message that says nothing about the
    // real problem, and the status line does.
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

/// Reduces a model's reply to one line of `title by artist`.
///
/// Written against what these models actually return for a prompt like
/// [`INSTRUCTION`]: the requested single line, sometimes quoted, sometimes with
/// a `Title:`/`Artist:` label on separate lines, and sometimes separated by a
/// dash. A reply of several unlabelled lines is refused rather than reduced,
/// because that is what a model hedging looks like, and the first line of a hedge
/// is a worse search query than the mishearing the stage was meant to fix.
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
    // A bare "Title:" names a field without filling it in, so it leaves the field
    // unset rather than setting it to nothing.
    let title = title.filter(|value| !value.is_empty());
    let artist = artist.filter(|value| !value.is_empty());

    let joined = match (title, artist) {
        (Some(title), Some(artist)) => format!("{title} by {artist}"),
        (Some(title), None) => title,
        (None, Some(artist)) => artist,
        (None, None) if unlabelled.len() == 1 => unlabelled.remove(0),
        // No labels, and not exactly one line: a hedge or an explanation.
        (None, None) => return None,
    };

    // A model asked for "<title> by <artist>" often answers with a dash between
    // them instead. The grammar splits on " by ", so the dash is translated
    // rather than searched for. Only the en and em dashes: a plain hyphen is
    // part of names like "Jay-Z" and has to survive.
    let joined = joined.replace(['\u{2014}', '\u{2013}'], " by ");
    // Translating a dash leaves the spaces that surrounded it doubled, and the
    // grammar splits on a single one.
    let joined = unquote(joined.trim());
    let joined = joined.split_whitespace().collect::<Vec<_>>().join(" ");
    if joined.is_empty() || joined.len() > MAX_REPLY_CHARS {
        return None;
    }
    Some(joined)
}

/// Which field a line names, if it names one.
///
/// Case-insensitively, because the label is the model's formatting rather than
/// anything it was asked for.
fn label_of(line: &str) -> Option<&'static str> {
    let lowered = line.to_ascii_lowercase();
    ["title", "artist"]
        .into_iter()
        .find(|label| lowered.starts_with(label))
}

/// What a label line puts after its colon, or nothing if it has no colon.
fn value_after_label(line: &str) -> String {
    line.split_once(':')
        .map(|(_, value)| value.trim().to_string())
        .unwrap_or_default()
}

/// Strips one layer of matching quotes and the trailing punctuation that goes
/// with them.
///
/// A full stop is deliberately left alone: names like "Run-D.M.C." end in one,
/// and a search query does not care about a trailing full stop.
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
