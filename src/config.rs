use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use std::{
    collections::HashSet,
    net::SocketAddr,
    path::{Path, PathBuf},
};
use url::Url;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub database: PathBuf,
    pub encryption_key_env: String,
    #[serde(default = "listen_default")]
    pub listen: SocketAddr,
    #[serde(default = "poll_default")]
    pub poll_seconds: u64,
    /// Matrix user allowed to write into bridged rooms.
    pub owner: String,
    pub homeserver: Url,
    pub registration: PathBuf,
    /// Stable tenant id used in ghost IDs and database keys.
    pub tenant: String,
    /// Channels to bridge. Empty means every inbound channel.
    #[serde(default)]
    pub channels: Vec<u32>,
    /// Contact pubkey prefixes to bridge. Empty means every inbound DM.
    #[serde(default)]
    pub contacts: Vec<String>,
    #[serde(default = "auto_create_default")]
    pub auto_create_rooms: bool,
    /// Replay the sidecar's historical message log on first run. Off by
    /// default: a fresh bridge starts at the latest message.
    #[serde(default)]
    pub backfill: bool,
    pub meshcore: MeshCoreConfig,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MeshCoreConfig {
    pub url: Url,
    /// Environment variable holding the sidecar bearer token.
    pub token_env: String,
    #[serde(default)]
    pub default_channel: u32,
}

fn listen_default() -> SocketAddr {
    "127.0.0.1:8080".parse().unwrap()
}
fn poll_default() -> u64 {
    2
}
fn auto_create_default() -> bool {
    true
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let mut config: Self = toml::from_str(
            &std::fs::read_to_string(path)
                .with_context(|| format!("read bridge config {}", path.display()))?,
        )
        .context("invalid bridge configuration")?;
        let root = path.parent().unwrap_or(Path::new("."));
        if config.database.is_relative() {
            config.database = root.join(&config.database);
        }
        if config.registration.is_relative() {
            config.registration = root.join(&config.registration);
        }
        ensure!(
            valid_slug(&config.tenant),
            "tenant must be a lowercase slug"
        );
        ensure!(
            config.owner.starts_with('@') && config.owner.contains(':'),
            "owner must be a Matrix user id"
        );
        ensure!(config.poll_seconds >= 1, "poll_seconds must be positive");
        ensure!(
            matches!(config.homeserver.scheme(), "http" | "https"),
            "homeserver must be http(s)"
        );
        ensure!(
            matches!(config.meshcore.url.scheme(), "http" | "https"),
            "meshcore.url must be http(s)"
        );
        ensure!(
            !config.encryption_key_env.is_empty(),
            "encryption_key_env must name the database key"
        );
        ensure!(
            !config.meshcore.token_env.is_empty(),
            "meshcore.token_env must name the sidecar token"
        );
        let mut seen = HashSet::new();
        for channel in &config.channels {
            ensure!(seen.insert(*channel), "duplicate channel {channel}");
        }
        let mut seen = HashSet::new();
        for contact in &config.contacts {
            ensure!(!contact.is_empty(), "empty contact prefix");
            ensure!(
                seen.insert(contact.to_lowercase()),
                "duplicate contact {contact}"
            );
            ensure!(
                contact.chars().all(|c| c.is_ascii_hexdigit()),
                "contact must be a hex public-key prefix"
            );
        }
        Ok(config)
    }

    pub fn allows_channel(&self, channel: i64) -> bool {
        if !(0..=u32::MAX as i64).contains(&channel) {
            return false;
        }
        self.channels.is_empty() || self.channels.contains(&(channel as u32))
    }

    pub fn allows_contact(&self, prefix: &str) -> bool {
        self.contacts.is_empty()
            || self
                .contacts
                .iter()
                .any(|p| prefix.to_lowercase().starts_with(&p.to_lowercase()))
    }
}

fn valid_slug(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        Config {
            database: "bridge.db".into(),
            encryption_key_env: "KEY".into(),
            listen: "127.0.0.1:8080".parse().unwrap(),
            poll_seconds: 2,
            owner: "@owner:matrix.example".into(),
            homeserver: "https://matrix.example".parse().unwrap(),
            registration: "registration.json".into(),
            tenant: "home".into(),
            channels: vec![0],
            contacts: vec!["dc6c06a8050b".into()],
            auto_create_rooms: true,
            backfill: false,
            meshcore: MeshCoreConfig {
                url: "http://127.0.0.1:8787".parse().unwrap(),
                token_env: "MESHCORE_TOKEN".into(),
                default_channel: 0,
            },
        }
    }

    #[test]
    fn channel_and_contact_allowlists_are_prefix_based() {
        let config = config();
        assert!(config.allows_channel(0));
        assert!(!config.allows_channel(1));
        assert!(config.allows_contact("dc6c06a8050b3f40"));
        assert!(!config.allows_contact("aa"));
    }

    #[test]
    fn empty_lists_mean_all() {
        let mut config = config();
        config.channels.clear();
        config.contacts.clear();
        assert!(config.allows_channel(7));
        assert!(config.allows_contact("anything"));
    }

    #[test]
    fn slugs_are_strict() {
        assert!(valid_slug("home-1"));
        assert!(!valid_slug("Home"));
        assert!(!valid_slug("a b"));
    }
}
