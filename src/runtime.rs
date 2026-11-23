//! The bridge runtime: MeshCore sidecar polling and Matrix transaction ingest.

use crate::{
    config::Config,
    meshcore::{Conversation, MeshCoreClient},
};
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use drawbridge_appservice::TransactionHandler;
use drawbridge_core::{RoomSpec, now, secret, stable_id};
use drawbridge_matrix::{MatrixAppService, RegistrationBundle, parse_transaction};
use drawbridge_store::{Binding, Store};
use serde_json::Value;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};
use tracing::{info, warn};

pub struct Bridge {
    config: Config,
    matrix: MatrixAppService,
    mesh: MeshCoreClient,
    store: Arc<Store>,
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
        let database_key = secret(&config.encryption_key_env)?;
        let store = Arc::new(Store::open(&config.database, &database_key, "")?);
        if let Err(error) = matrix.register_bot("MeshCore").await {
            warn!(%error, "could not register the bridge bot; continuing");
        }
        let bridge = Arc::new(Self {
            config,
            matrix,
            mesh,
            store,
            ready: AtomicBool::new(false),
            last_success: AtomicU64::new(0),
        });
        // Make the configured channel visible in Beeper before the first message.
        if bridge.config.auto_create_rooms
            && let Err(error) = bridge
                .ensure_binding(&Conversation::Channel(
                    bridge.config.meshcore.default_channel,
                ))
                .await
        {
            warn!(%error, "could not pre-create the default channel room");
        }
        Ok((bridge, hs_token))
    }

    fn conversation_allowed(&self, conversation: &Conversation) -> bool {
        match conversation {
            Conversation::Channel(channel) => self.config.allows_channel(*channel as i64),
            Conversation::Contact(prefix) => self.config.allows_contact(prefix),
        }
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
