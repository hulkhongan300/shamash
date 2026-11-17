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

#[test]
fn a_trailing_full_stop_is_left_alone() {
    assert_eq!(
        clean("Run-D.M.C. by Run-D.M.C.").as_deref(),
        Some("Run-D.M.C. by Run-D.M.C.")
    );
}

/// What models produce when they miss the single-line format.
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

#[test]
fn a_hyphen_is_part_of_the_name() {
    assert_eq!(
        clean("Jay-Z by Run-D.M.C.").as_deref(),
        Some("Jay-Z by Run-D.M.C.")
    );
}

#[test]
fn a_heading_is_not_mistaken_for_the_answer() {
    assert_eq!(
        clean("Song request:\nTitle: Dracula").as_deref(),
        Some("Dracula")
    );
}

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

#[test]
fn a_runaway_reply_is_refused() {
    let rambling = "a".repeat(MAX_REPLY_CHARS + 1);
    assert_eq!(clean(&rambling), None);
}

#[tokio::test]
async fn an_unreachable_endpoint_leaves_the_transcript_alone() {
    // Nothing listens on port 1, so the connection is refused at once.
    let http = reqwest::Client::new();
    let outcome = rewrite(&http, "http://127.0.0.1:1", "llama3.2", "hoolig bts").await;
    assert!(outcome.is_err(), "a dead endpoint is reported, not hidden");
}

#[tokio::test]
async fn an_empty_transcript_is_never_sent() {
    let http = reqwest::Client::new();
    let outcome = rewrite(&http, "http://127.0.0.1:1", "llama3.2", "   ")
        .await
        .unwrap();
    assert!(outcome.is_none());
}

/// Regression: the body used to be decoded first, hiding the status behind "chat reply".
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

/// Loopback server on an ephemeral port that answers once with a fixed status and body.
async fn one_shot_endpoint(status: u16, body: &'static str) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind a loopback port");
    let address = listener.local_addr().expect("the bound address");
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("a request");
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
