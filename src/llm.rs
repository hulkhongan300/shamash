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
mod tests {
    use super::*;

    #[test]
    fn the_requested_format_is_passed_through() {
        assert_eq!(
            clean("Dracula by Tame Impala").as_deref(),
            Some("Dracula by Tame Impala")
        );
    }

    #[test]
    fn quotes_and_trailing_punctuation_are_stripped() {
        assert_eq!(
            clean("\"Hooligan by BTS\"").as_deref(),
            Some("Hooligan by BTS")
        );
        assert_eq!(
            clean("Hooligan by BTS!").as_deref(),
            Some("Hooligan by BTS")
        );
    }

    /// A trailing full stop is left in place, because a name can end in one and
    /// a search query does not mind.
    #[test]
    fn a_trailing_full_stop_is_left_alone() {
        assert_eq!(
            clean("Run-D.M.C. by Run-D.M.C.").as_deref(),
            Some("Run-D.M.C. by Run-D.M.C.")
        );
    }

    /// The shape these models produce when the single-line format is missed.
    #[test]
    fn labelled_lines_are_combined() {
        assert_eq!(
            clean("Title: Dracula\nArtist: Tame Impala").as_deref(),
            Some("Dracula by Tame Impala")
        );
    }

    #[test]
    fn a_title_on_its_own_is_usable() {
        assert_eq!(clean("Title: Dracula").as_deref(), Some("Dracula"));
    }

    #[test]
    fn a_dash_is_read_as_by() {
        assert_eq!(
            clean("Dracula \u{2014} Tame Impala").as_deref(),
            Some("Dracula by Tame Impala")
        );
    }

    /// A hyphen is part of the name, so it must not be translated the way an em
    /// or en dash is.
    #[test]
    fn a_hyphen_is_part_of_the_name() {
        assert_eq!(
            clean("Jay-Z by Run-D.M.C.").as_deref(),
            Some("Jay-Z by Run-D.M.C.")
        );
    }

    /// Only the labelled parts of a reply with a heading are used, so the
    /// heading itself cannot end up in the search query.
    #[test]
    fn a_heading_is_not_mistaken_for_the_answer() {
        assert_eq!(
            clean("Song request:\nTitle: Dracula").as_deref(),
            Some("Dracula")
        );
    }

    /// A model that explains itself has not answered the question, and passing
    /// its commentary on would aim the search at a sentence. Only the first line
    /// of a hedge is worth keeping, and only when the rest are labels.
    #[test]
    fn commentary_is_refused() {
        assert_eq!(clean(""), None);
        assert_eq!(clean("   \n  \n"), None);
        assert_eq!(
            clean(
                "I'm not sure which song you mean.\n\
                 Dracula by Tame Impala is a popular track from 2016.\n\
                 Could you confirm?"
            ),
            None
        );
    }

    #[test]
    fn an_empty_reply_is_no_answer() {
        assert_eq!(clean("Title:"), None);
        assert_eq!(clean("Title:  \nArtist:  "), None);
    }

    /// The length bound is what stops a verbose reply becoming a query that
    /// matches nothing.
    #[test]
    fn a_runaway_reply_is_refused() {
        let rambling = "a".repeat(MAX_REPLY_CHARS + 1);
        assert_eq!(clean(&rambling), None);
    }

    #[tokio::test]
    async fn an_unreachable_endpoint_leaves_the_transcript_alone() {
        // Port 1 is reserved and nothing listens on it, so the connection is
        // refused immediately: the same path a model that is not running takes.
        let http = reqwest::Client::new();
        let outcome = rewrite(&http, "http://127.0.0.1:1", "llama3.2", "hoolig bts").await;
        assert!(outcome.is_err(), "a dead endpoint is reported, not hidden");
    }

    #[tokio::test]
    async fn an_empty_transcript_is_never_sent() {
        // Nothing to ask about, so no request is made and no failure is
        // reported; the caller keeps what it had.
        let http = reqwest::Client::new();
        let outcome = rewrite(&http, "http://127.0.0.1:1", "llama3.2", "   ")
            .await
            .unwrap();
        assert!(outcome.is_none());
    }

    /// A failed HTTP status must be reported as itself.
    ///
    /// The body used to be decoded before the status was looked at, so a 500
    /// from a proxy, an error page from a reverse proxy, or a 401 with an empty
    /// body all failed with "did not answer with a chat reply" -- a message that
    /// names the symptom rather than the cause, and sends you looking at the
    /// reply format instead of at the endpoint.
    #[tokio::test]
    async fn a_failed_status_is_reported_as_itself() {
        let endpoint = one_shot_endpoint(500, "upstream exploded").await;
        let err = rewrite(
            &reqwest::Client::new(),
            &endpoint,
            "llama3.2",
            "bot play hoolig bts",
        )
        .await
        .expect_err("a 500 is a failure");
        let message = format!("{err:#}");
        assert!(
            message.contains("500"),
            "the status should be named: {message}"
        );
        assert!(
            !message.contains("chat reply"),
            "the symptom must not be reported as the cause: {message}"
        );
    }

    /// And a 200 that is not a chat reply is still reported as what it is.
    #[tokio::test]
    async fn a_200_that_is_not_a_chat_reply_is_still_a_decode_failure() {
        let endpoint = one_shot_endpoint(200, "<html>hello</html>").await;
        let err = rewrite(
            &reqwest::Client::new(),
            &endpoint,
            "llama3.2",
            "bot play hoolig bts",
        )
        .await
        .expect_err("an HTML body is not a chat reply");
        assert!(format!("{err:#}").contains("chat reply"), "{err:#}");
    }

    /// The happy path, so the status check above is not the only thing tested.
    #[tokio::test]
    async fn a_chat_reply_becomes_a_request() {
        let endpoint = one_shot_endpoint(
            200,
            r#"{"message":{"role":"assistant","content":"Hooligan by BTS"}}"#,
        )
        .await;
        assert_eq!(
            rewrite(
                &reqwest::Client::new(),
                &endpoint,
                "llama3.2",
                "bot play hoolig bts"
            )
            .await
            .expect("a chat reply"),
            Some("Hooligan by BTS".to_string())
        );
    }

    /// A throwaway HTTP server that answers every request with one fixed
    /// status and body.
    ///
    /// Bound to an ephemeral port on loopback, so it cannot collide with
    /// anything and needs no configuration.
    async fn one_shot_endpoint(status: u16, body: &'static str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a loopback port");
        let address = listener.local_addr().expect("the bound address");
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("a request");
            // Read the request far enough that the client has finished sending.
            let mut request = [0u8; 1024];
            let _ = stream.read(&mut request).await;
            let response = format!(
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
            let _ = stream.flush().await;
        });
        format!("http://{address}")
    }
}
