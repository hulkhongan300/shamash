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

/// Guilds whose voice listening has been switched off with `/lock`.
///
/// While a guild is locked the bot stays in the channel and keeps playing, but
/// drops everything it hears without running a model on it. Held in memory
/// only: a restart starts listening again.
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
mod tests {
    use super::*;

    #[test]
    fn locking_is_per_guild_and_reports_changes() {
        let lock = ListenLock::default();
        let a = GuildId::new(1);
        let b = GuildId::new(2);
        assert!(!lock.is_locked(a));
        assert!(lock.set(a, true));
        assert!(!lock.set(a, true), "already locked");
        assert!(lock.is_locked(a));
        assert!(!lock.is_locked(b));
        assert!(lock.set(a, false));
        assert!(!lock.is_locked(a));
    }
}
