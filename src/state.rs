use crate::config::Config;
use crate::player::Player;
use crate::transcriber::Transcriber;
use crate::wake::BigModels;
use serenity::model::id::GuildId;
use serenity::prelude::TypeMapKey;
use std::sync::Arc;

/// Type-map key holding the runtime [`Config`].
pub struct ConfigKey;

impl TypeMapKey for ConfigKey {
    type Value = Config;
}

/// Type-map key for the one player shared by the voice listener and dashboard buttons.
pub struct PlayerKey;

impl TypeMapKey for PlayerKey {
    type Value = std::sync::Arc<Player>;
}

/// Type-map key holding the shared HTTP client used for music lookups.
pub struct HttpClientKey;

impl TypeMapKey for HttpClientKey {
    type Value = reqwest::Client;
}

/// Type-map key for the small wake-word model, held for the process since it runs on every window.
pub struct TranscriberKey;

impl TypeMapKey for TranscriberKey {
    type Value = Arc<dyn Transcriber>;
}

/// Type-map key holding the larger model, loaded when a wake word arrives.
pub struct BigModelsKey;

impl TypeMapKey for BigModelsKey {
    type Value = Arc<BigModels>;
}

/// Guilds with listening switched off by `/lock`: playback continues, speech is dropped. In memory only.
#[derive(Debug, Clone, Default)]
pub struct ListenLock(Arc<std::sync::Mutex<std::collections::HashSet<GuildId>>>);

impl ListenLock {
    pub fn is_locked(&self, guild_id: GuildId) -> bool {
        self.lock().contains(&guild_id)
    }

    /// Locks or unlocks `guild_id`; returns whether that changed anything.
    pub fn set(&self, guild_id: GuildId, locked: bool) -> bool {
        let mut all = self.lock();
        if locked {
            all.insert(guild_id)
        } else {
            all.remove(&guild_id)
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, std::collections::HashSet<GuildId>> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Type-map key holding the [`ListenLock`].
pub struct ListenLockKey;

impl TypeMapKey for ListenLockKey {
    type Value = ListenLock;
}

#[cfg(test)]
#[path = "tests/state.rs"]
mod tests;
