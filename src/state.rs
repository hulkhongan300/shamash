use crate::config::Config;
use serenity::prelude::TypeMapKey;

/// Type-map key holding the runtime [`Config`].
pub struct ConfigKey;

impl TypeMapKey for ConfigKey {
    type Value = Config;
}

/// Type-map key holding the shared HTTP client used for music lookups.
pub struct HttpClientKey;

impl TypeMapKey for HttpClientKey {
    type Value = reqwest::Client;
}
