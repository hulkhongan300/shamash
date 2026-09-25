use crate::config::Config;
use crate::transcriber::Transcriber;
use serenity::prelude::TypeMapKey;
use std::sync::Arc;

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

/// Type-map key holding the shared speech-to-text transcriber.
pub struct TranscriberKey;

impl TypeMapKey for TranscriberKey {
    type Value = Arc<dyn Transcriber>;
}
