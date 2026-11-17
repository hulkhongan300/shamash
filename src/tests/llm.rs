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
