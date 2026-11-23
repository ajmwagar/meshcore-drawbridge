//! Thin client for the Radioman MeshCore sidecar's loopback HTTP API.
//!
//! The sidecar is the single owner of the companion radio. This bridge never
//! touches BLE or the radio's contact table; it polls the sidecar's durable
//! message log and sends through its bounded, allowlisted `/v1/send` endpoint.
//! That separation is what lets the network adapter be replaced or scaled
//! independently of Matrix.

use anyhow::{Context, Result, bail, ensure};
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::HashMap, time::Duration};
use url::Url;

#[derive(Clone)]
pub struct MeshCoreClient {
    base: Url,
    token: String,
    http: Client,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Health {
    pub schema: String,
    pub device_id: String,
    pub connected: bool,
    pub public_key: String,
    pub name: String,
    pub contacts: u64,
    pub channels: u64,
    #[serde(default)]
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Message {
    pub id: i64,
    pub message_id: String,
    pub direction: String,
    pub channel: Option<i64>,
    pub contact: Option<String>,
    pub sender_name: Option<String>,
    pub text: String,
    #[serde(default)]
    pub sent_at: Option<u64>,
    #[serde(default)]
    pub received_at: u64,
}

impl Message {
    pub fn conversation(&self) -> Option<Conversation> {
        match (self.channel, self.contact.as_deref()) {
            (Some(channel), _) if (0..=u32::MAX as i64).contains(&channel) => {
                Some(Conversation::Channel(channel as u32))
            }
            (_, Some(contact)) if !contact.is_empty() => {
                Some(Conversation::Contact(contact.to_lowercase()))
            }
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Conversation {
    Channel(u32),
    Contact(String),
}

impl Conversation {
    pub fn key(&self) -> String {
        match self {
            Self::Channel(channel) => format!("channel:{channel}"),
            Self::Contact(prefix) => format!("contact:{prefix}"),
        }
    }

    pub fn parse(key: &str) -> Result<Self> {
        let (kind, value) = key
            .split_once(':')
            .context("conversation key must be kind:value")?;
        match kind {
            "channel" => Ok(Self::Channel(
                value.parse().context("invalid channel number")?,
            )),
            "contact" => {
                ensure!(
                    !value.is_empty() && value.chars().all(|c| c.is_ascii_hexdigit()),
                    "invalid contact prefix"
                );
                Ok(Self::Contact(value.to_lowercase()))
            }
            other => bail!("unknown conversation kind {other}"),
        }
    }

    pub fn display(&self, channel_name: Option<&str>, contact_name: Option<&str>) -> String {
        match self {
            Self::Channel(channel) => channel_name
                .filter(|name| !name.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| format!("MeshCore channel {channel}")),
            Self::Contact(prefix) => contact_name
                .filter(|name| !name.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| format!("MeshCore DM {prefix}")),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct SendAck {
    pub request_id: String,
    pub state: String,
}

impl MeshCoreClient {
    pub fn new(base: Url, token: impl Into<String>) -> Result<Self> {
        Ok(Self {
            base,
            token: token.into(),
            http: Client::builder()
                .timeout(Duration::from_secs(20))
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
        })
    }

    fn url(&self, path: &str, query: &[(&str, String)]) -> Result<Url> {
        let mut url = self.base.clone();
        url.path_segments_mut()
            .map_err(|_| anyhow::anyhow!("invalid MeshCore sidecar URL"))?
            .pop_if_empty()
            .extend(path.split('/').filter(|p| !p.is_empty()));
        if !query.is_empty() {
            url.query_pairs_mut()
                .extend_pairs(query.iter().map(|(k, v)| (*k, v.as_str())));
        }
        Ok(url)
    }

    async fn get(&self, path: &str, query: &[(&str, String)]) -> Result<Value> {
        let response = self
            .http
            .get(self.url(path, query)?)
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("MeshCore sidecar request failed: {}", e.without_url()))?;
        self.decode(response, path).await
    }

    async fn post(&self, path: &str, body: Value) -> Result<Value> {
        let response = self
            .http
            .post(self.url(path, &[])?)
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("MeshCore sidecar request failed: {}", e.without_url()))?;
        self.decode(response, path).await
    }

    async fn decode(&self, response: reqwest::Response, path: &str) -> Result<Value> {
        let status = response.status();
        let body: Value = response.json().await.unwrap_or(Value::Null);
        if status.is_success() {
            return Ok(body);
        }
        let detail = body
            .get("error")
            .and_then(Value::as_str)
            .or_else(|| body.as_str())
            .unwrap_or("");
        match status {
            StatusCode::UNAUTHORIZED => {
                bail!("MeshCore sidecar rejected the bearer token ({path})")
            }
            StatusCode::FORBIDDEN => {
                bail!("MeshCore sidecar refused {path}: channel or contact is not allowlisted")
            }
            StatusCode::TOO_MANY_REQUESTS => bail!("MeshCore sidecar rate limit hit on {path}"),
            _ => bail!("MeshCore sidecar {path} returned HTTP {status}: {detail}"),
        }
    }

    pub async fn health(&self) -> Result<Health> {
        let value = self.get("/v1/health", &[]).await?;
        serde_json::from_value(value).context("invalid sidecar health response")
    }

    pub async fn messages(&self, since: i64, limit: u32) -> Result<Vec<Message>> {
        let value = self
            .get(
                "/v1/messages",
                &[
                    ("since", since.to_string()),
                    ("limit", limit.min(500).to_string()),
                ],
            )
            .await?;
        let messages = value
            .get("messages")
            .cloned()
            .context("sidecar messages response missing messages")?;
        serde_json::from_value(messages).context("invalid sidecar messages response")
    }

    /// Highest message id currently in the sidecar log. Used to start a new
    /// bridge at "now" instead of replaying the entire history.
    pub async fn latest_id(&self) -> Result<i64> {
        let mut since = 0i64;
        let mut latest = 0i64;
        loop {
            let page = self.messages(since, 500).await?;
            if page.is_empty() {
                break;
            }
            let max = page.iter().map(|message| message.id).max().unwrap_or(since);
            latest = latest.max(max);
            if page.len() < 500 {
                break;
            }
            since = latest;
        }
        Ok(latest)
    }

    pub async fn contacts(&self) -> Result<HashMap<String, Value>> {
        let value = self.get("/v1/contacts", &[]).await?;
        serde_json::from_value(
            value
                .get("contacts")
                .cloned()
                .context("sidecar contacts response missing contacts")?,
        )
        .context("invalid sidecar contacts response")
    }

    pub async fn channels(&self) -> Result<HashMap<String, Value>> {
        let value = self.get("/v1/channels", &[]).await?;
        serde_json::from_value(
            value
                .get("channels")
                .cloned()
                .context("sidecar channels response missing channels")?,
        )
        .context("invalid sidecar channels response")
    }

    pub fn channel_name(channels: &HashMap<String, Value>, channel: u32) -> Option<String> {
        let value = channels.get(&channel.to_string())?;
        if let Some(name) = value.as_str() {
            return Some(name.to_string());
        }
        value
            .get("name")
            .or_else(|| value.get("label"))
            .and_then(Value::as_str)
            .map(str::to_string)
    }

    pub fn contact_name(contacts: &HashMap<String, Value>, prefix: &str) -> Option<String> {
        contacts
            .iter()
            .find(|(key, value)| {
                key.to_lowercase().starts_with(&prefix.to_lowercase())
                    || value["public_key"]
                        .as_str()
                        .is_some_and(|key| key.to_lowercase().starts_with(&prefix.to_lowercase()))
            })
            .and_then(|(_, value)| value.get("adv_name"))
            .and_then(Value::as_str)
            .map(str::to_string)
    }

    pub async fn send_channel(&self, channel: u32, text: &str) -> Result<SendAck> {
        let value = self
            .post("/v1/send", json!({"channel": channel, "text": text}))
            .await?;
        parse_ack(value)
    }

    pub async fn send_contact(&self, prefix: &str, text: &str) -> Result<SendAck> {
        let value = self
            .post("/v1/send", json!({"contact": prefix, "text": text}))
            .await?;
        parse_ack(value)
    }

    pub async fn send(&self, conversation: &Conversation, text: &str) -> Result<SendAck> {
        match conversation {
            Conversation::Channel(channel) => self.send_channel(*channel, text).await,
            Conversation::Contact(prefix) => self.send_contact(prefix, text).await,
        }
    }
}

fn parse_ack(value: Value) -> Result<SendAck> {
    serde_json::from_value(value).context("invalid sidecar send response")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversations_round_trip() {
        for conversation in [
            Conversation::Channel(0),
            Conversation::Contact("dc6c06a8050b".into()),
        ] {
            assert_eq!(
                Conversation::parse(&conversation.key()).unwrap(),
                conversation
            );
        }
    }

    #[test]
    fn conversation_display_prefers_names() {
        assert_eq!(
            Conversation::Channel(0).display(Some("Public"), None),
            "Public"
        );
        assert_eq!(
            Conversation::Contact("ab".into()).display(None, Some("Dad")),
            "Dad"
        );
    }

    #[test]
    fn bad_conversation_keys_are_rejected() {
        assert!(Conversation::parse("channel").is_err());
        assert!(Conversation::parse("channel:no").is_err());
        assert!(Conversation::parse("contact:zz").is_err());
        assert!(Conversation::parse("other:1").is_err());
    }

    #[test]
    fn messages_choose_the_right_conversation() {
        let channel = Message {
            id: 1,
            message_id: "m".into(),
            direction: "inbound".into(),
            channel: Some(0),
            contact: None,
            sender_name: None,
            text: "hi".into(),
            sent_at: None,
            received_at: 0,
        };
        assert_eq!(channel.conversation(), Some(Conversation::Channel(0)));
    }
}
