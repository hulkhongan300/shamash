use crate::config::Config;
use crate::player::Player;
use crate::transcriber::Transcriber;
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

/// Type-map key holding the shared speech-to-text transcriber.
pub struct TranscriberKey;

impl TypeMapKey for TranscriberKey {
    type Value = Arc<dyn Transcriber>;
}
