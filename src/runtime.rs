//! The bridge runtime: MeshCore sidecar polling and Matrix transaction ingest.

use crate::{
    commands::{self, Command},
    config::Config,
    meshcore::{Conversation, MeshCoreClient},
};
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use drawbridge_appservice::TransactionHandler;
use drawbridge_core::{RoomSpec, now, secret, stable_id};
use drawbridge_matrix::{MatrixAppService, RegistrationBundle, parse_transaction};
use drawbridge_store::{Binding, Store};
use reqwest::{
    Client,
    header::{AUTHORIZATION, HeaderValue},
};
use serde_json::Value;
use std::{
    collections::HashSet,
    sync::{
        Arc, RwLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};
use tracing::{info, warn};

/// Minimal owner-scoped join client. Drawbridge owns the canonical
/// `MatrixUser` implementation; this shim keeps the bridge buildable against the
/// currently pinned Drawbridge revision until that commit is published.
#[derive(Clone)]
struct OwnerJoin {
    client: Client,
    base: url::Url,
    token: HeaderValue,
}

impl OwnerJoin {
    fn new(homeserver: &url::Url, token: &str) -> Result<Self> {
        let mut token = HeaderValue::from_str(&format!("Bearer {token}"))?;
        token.set_sensitive(true);
        Ok(Self {
            client: Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(30))
                .build()?,
            base: homeserver.clone(),
            token,
        })
    }

    async fn join(&self, room: &str) -> Result<()> {
        let mut url = self.base.clone();
        url.path_segments_mut()
            .map_err(|_| anyhow::anyhow!("invalid Matrix base URL"))?
            .pop_if_empty()
            .extend(["_matrix", "client", "v3", "join", room]);
        let response = self
            .client
            .post(url)
            .header(AUTHORIZATION, self.token.clone())
            .json(&serde_json::json!({}))
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("Matrix join failed: {}", e.without_url()))?;
        let status = response.status();
        let body: Value = response.json().await.unwrap_or(Value::Null);
        if !status.is_success() && status != reqwest::StatusCode::CONFLICT {
            bail!("Matrix join returned HTTP {status}: {body}");
        }
        Ok(())
    }
}

pub struct Bridge {
    config: Config,
    matrix: MatrixAppService,
    mesh: MeshCoreClient,
    store: Arc<Store>,
    allowed_channels: RwLock<HashSet<u32>>,
    owner: Option<OwnerJoin>,
    ready: AtomicBool,
    last_success: AtomicU64,
}

impl Bridge {
    pub async fn new(config: Config) -> Result<(Arc<Self>, String)> {
        let bundle = RegistrationBundle::load(&config.registration)?;
        let matrix = MatrixAppService::new(&config.homeserver, &bundle)?;
        let hs_token = bundle.registration.hs_token.clone();
        let mesh_token = secret(&config.meshcore.token_env)?;
        let mesh = MeshCoreClient::new(config.meshcore.url.clone(), mesh_token)?;
        let owner = match config.owner_token_env.as_deref() {
            Some(env) => Some(OwnerJoin::new(&config.homeserver, &secret(env)?)?),
            None => None,
        };
        let database_key = secret(&config.encryption_key_env)?;
        let store = Arc::new(Store::open(&config.database, &database_key, "")?);
        let mut allowed_channels: HashSet<u32> = config.channels.iter().copied().collect();
        for binding in store.bindings(&config.tenant)? {
            if let Ok(Conversation::Channel(channel)) = Conversation::parse(&binding.key) {
                allowed_channels.insert(channel);
            }
        }
        if let Err(error) = matrix.register_bot("MeshCore").await {
            warn!(%error, "could not register the bridge bot; continuing");
        }
        let bridge = Arc::new(Self {
            config,
            matrix,
            mesh,
            store,
            allowed_channels: RwLock::new(allowed_channels),
            owner,
            ready: AtomicBool::new(false),
            last_success: AtomicU64::new(0),
        });
        // A fresh bridge starts at the latest sidecar message unless the
        // operator explicitly asks for history.
        if bridge.store.cursor(&bridge.config.tenant, "meshcore")? == 0 && !bridge.config.backfill {
            match bridge.mesh.latest_id().await {
                Ok(latest) if latest > 0 => {
                    bridge
                        .store
                        .set_cursor(&bridge.config.tenant, "meshcore", latest)?;
                    info!(latest, "starting at the latest MeshCore message");
                }
                Ok(_) => {}
                Err(error) => {
                    warn!(%error, "could not determine the latest MeshCore message; starting at 0");
                }
            }
        }
        // Opt-in owner auto-join for rooms that already exist (for example after
        // a restart, or when the bridge was previously invite-only).
        if let Some(owner) = &bridge.owner {
            for binding in bridge.store.bindings(&bridge.config.tenant)? {
                if let Err(error) = owner.join(&binding.room).await {
                    warn!(%error, room=%binding.room, "owner auto-join failed");
                }
            }
        }
        // Make every configured channel visible in Beeper before its first message.
        if bridge.config.auto_create_rooms {
            let mut channels = bridge.config.channels.clone();
            if !channels.contains(&bridge.config.meshcore.default_channel) {
                channels.push(bridge.config.meshcore.default_channel);
            }
            for channel in channels {
                if let Err(error) = bridge.ensure_binding(&Conversation::Channel(channel)).await {
                    warn!(%error, channel, "could not pre-create channel room");
                }
            }
        }
        Ok((bridge, hs_token))
    }

    fn conversation_allowed(&self, conversation: &Conversation) -> bool {
        match conversation {
            Conversation::Channel(channel) => {
                self.config.channels.is_empty()
                    || self
                        .allowed_channels
                        .read()
                        .expect("allowed_channels poisoned")
                        .contains(channel)
            }
            Conversation::Contact(prefix) => self.config.allows_contact(prefix),
        }
    }

    fn allow_channel(&self, channel: u32) {
        self.allowed_channels
            .write()
            .expect("allowed_channels poisoned")
            .insert(channel);
    }

    async fn ensure_binding(&self, conversation: &Conversation) -> Result<Option<Binding>> {
        let key = conversation.key();
        if let Some(binding) = self.store.binding(&self.config.tenant, &key)? {
            return Ok(Some(binding));
        }
        if !self.config.auto_create_rooms {
            return Ok(None);
        }
        let display = match conversation {
            Conversation::Channel(channel) => {
                let channels = self.mesh.channels().await.unwrap_or_default();
                conversation.display(
                    MeshCoreClient::channel_name(&channels, *channel).as_deref(),
                    None,
                )
            }
            Conversation::Contact(prefix) => {
                let contacts = self.mesh.contacts().await.unwrap_or_default();
                conversation.display(
                    None,
                    MeshCoreClient::contact_name(&contacts, prefix).as_deref(),
                )
            }
        };
        let ghost = self.matrix.ghost_for(&[&self.config.tenant, &key])?;
        self.matrix.register_user(&ghost, &display).await?;
        let spec = RoomSpec {
            name: format!("{display} · MeshCore"),
            topic: "MeshCore conversation bridged by meshcore-mautrix. This bridge room is not end-to-end encrypted.".into(),
            bridge_key: format!("meshcore://{}/{}", self.config.tenant, key),
            protocol_id: "meshcore".into(),
            protocol_display_name: "MeshCore".into(),
            channel_id: key.clone(),
            channel_display_name: display.clone(),
        };
        let room = self
            .matrix
            .ensure_room(&spec, &ghost, &self.config.owner)
            .await?;
        if let Some(owner) = &self.owner {
            if let Err(error) = owner.join(&room).await {
                warn!(%error, %room, "owner auto-join failed");
            }
        }
        let binding = Binding {
            tenant: self.config.tenant.clone(),
            key,
            room,
            ghost,
            display,
        };
        self.store.upsert_binding(&binding)?;
        info!(conversation=%binding.key, room=%binding.room, "created MeshCore room");
        Ok(Some(binding))
    }

    /// Pull one page from the sidecar. The sidecar's cursor is the durable
    /// message id, not a timestamp: a restart resumes exactly where it stopped.
    pub async fn poll_once(&self) -> Result<usize> {
        let cursor = self.store.cursor(&self.config.tenant, "meshcore")?;
        let messages = self.mesh.messages(cursor, 100).await?;
        let mut next_cursor = cursor;
        let mut delivered = 0;
        for message in messages {
            next_cursor = next_cursor.max(message.id);
            if message.direction != "inbound" {
                continue;
            }
            let Some(conversation) = message.conversation() else {
                continue;
            };
            if !self.conversation_allowed(&conversation) {
                continue;
            }
            if self
                .store
                .linked(&self.config.tenant, &message.message_id)?
            {
                continue;
            }
            let Some(binding) = self.ensure_binding(&conversation).await? else {
                continue;
            };
            let transaction = stable_id(&[&self.config.tenant, "meshcore", &message.message_id]);
            let event_id = self
                .matrix
                .send_text(&binding.room, &binding.ghost, &transaction, &message.text)
                .await
                .with_context(|| format!("deliver {} to Matrix", message.message_id))?;
            self.store.link(
                &self.config.tenant,
                &message.message_id,
                &event_id,
                "inbound",
                &binding.key,
            )?;
            delivered += 1;
        }
        if next_cursor > cursor {
            self.store
                .set_cursor(&self.config.tenant, "meshcore", next_cursor)?;
        }
        if delivered > 0 {
            info!(delivered, cursor = next_cursor, "polled MeshCore");
        }
        self.last_success.store(now(), Ordering::Relaxed);
        self.ready.store(true, Ordering::Relaxed);
        Ok(delivered)
    }

    pub async fn poll_loop(self: Arc<Self>) {
        loop {
            match self.poll_once().await {
                Ok(_) => {}
                Err(error) => {
                    warn!(%error, "MeshCore poll failed");
                    self.ready.store(false, Ordering::Relaxed);
                }
            }
            tokio::time::sleep(Duration::from_secs(self.config.poll_seconds)).await;
        }
    }

    async fn ingest_events(&self, payload: &Value) -> Result<()> {
        for event in parse_transaction(payload)? {
            if event.encrypted() {
                warn!(room=%event.room_id, "encrypted Matrix event ignored");
                continue;
            }
            let Some(text) = event.text() else {
                continue;
            };
            if event.sender != self.config.owner {
                warn!(
                    sender=%event.sender,
                    room=%event.room_id,
                    "ignoring Matrix write from a non-owner"
                );
                continue;
            }
            let Some(binding) = self
                .store
                .binding_by_room(&self.config.tenant, &event.room_id)?
            else {
                continue;
            };
            if !self
                .store
                .claim_event(&self.config.tenant, &event.event_id)?
            {
                continue;
            }
            let conversation = Conversation::parse(&binding.key)
                .with_context(|| format!("invalid bound conversation {}", binding.key))?;
            if !self.conversation_allowed(&conversation) {
                warn!(conversation=%binding.key, "bound conversation is no longer allowed");
                continue;
            }
            if self.handle_command(&binding, &event.event_id, text).await? {
                self.store.link(
                    &self.config.tenant,
                    &event.event_id,
                    &format!("command:{}", binding.key),
                    "outbound",
                    &binding.key,
                )?;
                continue;
            }
            let ack = match self.mesh.send(&conversation, text).await {
                Ok(ack) => ack,
                Err(error) => {
                    // Release the claim so the homeserver retry can try again.
                    self.store
                        .release_event(&self.config.tenant, &event.event_id)?;
                    return Err(error).with_context(|| format!("send {} to MeshCore", binding.key));
                }
            };
            self.store.link(
                &self.config.tenant,
                &event.event_id,
                &format!("sidecar:{}", ack.request_id),
                "outbound",
                &binding.key,
            )?;
        }
        Ok(())
    }

    async fn handle_command(&self, binding: &Binding, event_id: &str, text: &str) -> Result<bool> {
        let Some(parsed) = commands::parse(text) else {
            return Ok(false);
        };
        let response = match parsed {
            Ok(command) => match self.run_command(command).await {
                Ok(message) => message,
                Err(error) => format!("Command failed: {error}"),
            },
            Err(usage) => usage,
        };
        let transaction = stable_id(&[&self.config.tenant, "command", event_id]);
        self.matrix
            .send_text(&binding.room, &binding.ghost, &transaction, &response)
            .await?;
        Ok(true)
    }

    async fn run_command(&self, command: Command) -> Result<String> {
        match command {
            Command::Help => Ok(commands::HELP.to_string()),
            Command::Status => {
                let health = self.mesh.health().await?;
                let key = &health.public_key[..health.public_key.len().min(12)];
                Ok(format!(
                    "{} · {} · {:.3} MHz BW {:.1} SF{} CR{} · {} dBm · contacts {} · channels {}",
                    health.name,
                    key,
                    health.radio.freq_mhz,
                    health.radio.bw_khz,
                    health.radio.sf,
                    health.radio.cr,
                    health.radio.tx_power,
                    health.contacts,
                    health.channels
                ))
            }
            Command::Advert { flood } => {
                self.mesh.advert(flood).await?;
                Ok(if flood {
                    "Flood advert sent.".into()
                } else {
                    "Zero-hop advert sent.".into()
                })
            }
            Command::Name(name) => {
                self.mesh.set_name(&name).await?;
                Ok(format!("Radio name set to {name}."))
            }
            Command::TxPower(value) => {
                self.mesh.set_tx_power(value).await?;
                Ok(format!("TX power set to {value} dBm."))
            }
            Command::PathMode(mode) => {
                self.mesh.set_path_hash_mode(mode).await?;
                Ok(format!(
                    "Message path hash mode set to {mode} ({} bytes).",
                    mode + 1
                ))
            }
            Command::AutoAdd(enabled) => {
                self.mesh.set_manual_add_contacts(!enabled).await?;
                Ok(format!(
                    "Contact auto-add {}.",
                    if enabled {
                        "enabled"
                    } else {
                        "disabled (manual add only)"
                    }
                ))
            }
            Command::Channels => {
                let channels = self.mesh.channels().await?;
                let mut names: Vec<(u32, String)> = channels
                    .iter()
                    .filter_map(|(index, value)| {
                        let index = index.parse().ok()?;
                        let name = value.get("name").and_then(Value::as_str).unwrap_or("");
                        Some((
                            index,
                            if name.is_empty() {
                                format!("#{index}")
                            } else {
                                format!("{name} (#{index})")
                            },
                        ))
                    })
                    .collect();
                names.sort();
                Ok(format!(
                    "Channels: {}",
                    names
                        .iter()
                        .map(|(_, name)| name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            }
            Command::ChannelCreate(name) => {
                let created = self.mesh.create_channel(&name).await?;
                self.allow_channel(created.channel_idx as u32);
                let binding = self
                    .ensure_binding(&Conversation::Channel(created.channel_idx as u32))
                    .await?;
                let room = binding
                    .map(|binding| binding.room)
                    .unwrap_or_else(|| "(room creation disabled)".into());
                Ok(format!(
                    "Created private channel #{} '{}'.\nKey: {}\nShare: {}\nRoom: {}\nAnyone with this key can read the channel.",
                    created.channel_idx, created.name, created.secret, created.uri, room
                ))
            }
            Command::ChannelKey(index) => {
                let channel = self.mesh.channel_key(index).await?;
                Ok(format!(
                    "Channel #{} '{}'.\nKey: {}\nShare: {}\nTreat this key as a secret.",
                    channel.channel_idx, channel.name, channel.secret, channel.uri
                ))
            }
            Command::ChannelShare(index) => {
                let channel = self.mesh.channel_key(index).await?;
                Ok(format!(
                    "Channel #{} '{}': {}",
                    channel.channel_idx, channel.name, channel.uri
                ))
            }
            Command::Contacts => {
                let contacts = self.mesh.contacts().await?;
                let chats = contacts
                    .values()
                    .filter(|value| value.get("type").and_then(Value::as_u64) == Some(1))
                    .count();
                Ok(format!(
                    "{} contacts on the radio; {chats} chat contacts. Use /mesh name or the Beeper room to DM a configured contact.",
                    contacts.len()
                ))
            }
        }
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    pub fn matrix(&self) -> &MatrixAppService {
        &self.matrix
    }
}

#[async_trait]
impl TransactionHandler for Bridge {
    async fn ingest(&self, txn: &str, payload: &Value) -> Result<()> {
        if !self.store.claim_transaction(txn)? {
            return Ok(());
        }
        if let Err(error) = self.ingest_events(payload).await {
            self.store.release_transaction(txn)?;
            return Err(error);
        }
        Ok(())
    }

    fn ready(&self) -> bool {
        self.ready.load(Ordering::Relaxed)
    }

    fn pending(&self) -> Option<u64> {
        self.store.pending_count().ok()
    }

    fn metric_prefix(&self) -> &'static str {
        "meshcore_mautrix"
    }
}

/// A small helper used by `check` to fail fast on configuration errors.
pub async fn check(config: &Config) -> Result<()> {
    let bundle = RegistrationBundle::load(&config.registration)?;
    let matrix = MatrixAppService::new(&config.homeserver, &bundle)?;
    let mesh_token = secret(&config.meshcore.token_env)?;
    let mesh = MeshCoreClient::new(config.meshcore.url.clone(), mesh_token)?;
    let health = mesh.health().await?;
    if !health.connected {
        bail!("MeshCore sidecar reports connected=false");
    }
    let _ = secret(&config.encryption_key_env)?;
    println!(
        "ok: matrix={} sender={} tenant={} mesh={} ({}) contacts={} channels={}",
        matrix.domain,
        matrix.sender,
        config.tenant,
        health.name,
        health.public_key,
        health.contacts,
        health.channels
    );
    Ok(())
}
