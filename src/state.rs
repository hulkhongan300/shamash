use crate::config::Config;
use crate::player::Player;
use crate::transcriber::Transcriber;
use crate::wake::BigModels;
use serenity::prelude::TypeMapKey;
use std::sync::Arc;

/// Type-map key holding the runtime [`Config`].
pub struct ConfigKey;

impl TypeMapKey for ConfigKey {
    type Value = Config;
}

/// Type-map key holding the shared music player.
///
/// One player is built at startup and used by both the voice listener and the
/// dashboard buttons, so the queue they act on is the same one.
pub struct PlayerKey;

impl TypeMapKey for PlayerKey {
    type Value = std::sync::Arc<Player>;
}

/// Type-map key holding the shared HTTP client used for music lookups.
pub struct HttpClientKey;

impl TypeMapKey for HttpClientKey {
    type Value = reqwest::Client;
}

/// Type-map key holding the small model that watches for the wake word.
///
/// Distinct from the larger model on purpose: this one is loaded at startup and
/// held for the life of the process, because it runs on every window of speech.
pub struct TranscriberKey;

impl TypeMapKey for TranscriberKey {
    type Value = Arc<dyn Transcriber>;
}

/// Type-map key holding the larger model, loaded when a wake word arrives.
pub struct BigModelsKey;

impl TypeMapKey for BigModelsKey {
    type Value = Arc<BigModels>;
}
