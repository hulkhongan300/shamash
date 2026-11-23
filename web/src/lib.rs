//! The bot's own parser and matcher, built for the browser; the page does the rest.

// The bot's files as they are, so the demo cannot drift from it.
#[allow(dead_code)]
#[path = "../../src/matcher.rs"]
mod matcher;
#[allow(dead_code)]
#[path = "../../src/parser.rs"]
mod parser;

use serde_json::{Value, json};
use wasm_bindgen::prelude::wasm_bindgen;

/// What the bot would make of `said`, as JSON; `catalog` is a JSON list of [artist, title].
#[wasm_bindgen]
pub fn run(said: &str, wake_word: &str, catalog: &str) -> String {
    let catalog: Vec<(String, String)> = serde_json::from_str(catalog).unwrap_or_default();
    report(said, wake_word, &catalog).to_string()
}

fn report(said: &str, wake_word: &str, catalog: &[(String, String)]) -> Value {
    let parser = parser::CommandParser::new([wake_word.to_string()]);
    if let Some(control) = parser.control(said) {
        return json!({ "woke": true, "control": format!("{control:?}").to_lowercase() });
    }
    let request = match parser.command(said) {
        Ok(request) => request,
        Err(miss) => {
            return json!({ "woke": miss != parser::ParseMiss::NoWakeWord, "miss": miss.to_string() });
        }
    };

    let heard = &request.query;
    let scores: Vec<Value> = catalog
        .iter()
        .map(|(artist, title)| {
            let s = matcher::score(heard, artist, title);
            json!({ "title": s.title, "artist": s.artist, "combined": s.combined })
        })
        .collect();
    let index = |hit: &(String, String)| catalog.iter().position(|c| std::ptr::eq(c, hit));
    let (verdict, picks) = match matcher::decide(heard, catalog, |(a, t)| (a.as_str(), t.as_str()))
    {
        matcher::Verdict::Confident(hit) => ("play", vec![index(hit)]),
        matcher::Verdict::Ambiguous(hits) => ("ask", hits.into_iter().map(index).collect()),
        matcher::Verdict::NoMatch => ("none", vec![]),
    };
    json!({
        "woke": true,
        "query": heard,
        "title": request.title,
        "artist": request.artist,
        "splits": request.splits,
        "scores": scores,
        "verdict": verdict,
        "picks": picks,
    })
}

#[cfg(test)]
mod demo_tests {
    use super::*;

    fn catalog() -> Vec<(String, String)> {
        [
            ("Elektronomia", "Sky High"),
            ("Elektronomia", "Limitless"),
            ("Lost Sky", "Fearless"),
            ("Lost Sky", "Fearless pt.II (feat. Chris Linton)"),
            ("DEAF KEV", "Invincible"),
        ]
        .map(|(a, t)| (a.to_string(), t.to_string()))
        .to_vec()
    }

    fn says(said: &str) -> Value {
        let json = run(said, "groovy", &serde_json::to_string(&catalog()).unwrap());
        serde_json::from_str(&json).unwrap()
    }

    #[test]
    fn a_misheard_request_still_plays_the_song() {
        let out = says("Roovy, could you play sky hi by electronomia");
        assert_eq!(out["verdict"], "play");
        assert_eq!(out["picks"], json!([0]));
    }

    #[test]
    fn no_wake_word_is_ignored() {
        let out = says("play sky high");
        assert_eq!(out["woke"], false);
        assert!(out["miss"].is_string());
    }

    #[test]
    fn a_control_word_is_not_a_song() {
        assert_eq!(says("groovy stop")["control"], "stop");
    }
}
