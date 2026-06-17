use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub type ChannelId = String;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ChannelType {
    Discord,
    Telegram,
    Slack,
    WebSocket,
    Webhook,
    Email,
    Sms,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ChannelStatus {
    Connected,
    Disconnected,
    Error,
    Paused,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Channel {
    pub id: ChannelId,
    pub name: String,
    pub channel_type: ChannelType,
    pub status: ChannelStatus,
    pub config: HashMap<String, String>,
    pub created_at: DateTime<Utc>,
    pub last_message_at: Option<DateTime<Utc>>,
    pub message_count: u64,
    pub metadata: HashMap<String, String>,
}

impl Channel {
    pub fn new(name: &str, channel_type: ChannelType) -> Self {
        Self {
            id: uuid::Uuid::now_v7().to_string(),
            name: name.to_string(),
            channel_type,
            status: ChannelStatus::Disconnected,
            config: HashMap::new(),
            created_at: Utc::now(),
            last_message_at: None,
            message_count: 0,
            metadata: HashMap::new(),
        }
    }

    pub fn connect(&mut self) {
        self.status = ChannelStatus::Connected;
    }

    pub fn disconnect(&mut self) {
        self.status = ChannelStatus::Disconnected;
    }

    pub fn pause(&mut self) {
        self.status = ChannelStatus::Paused;
    }

    pub fn record_message(&mut self) {
        self.message_count += 1;
        self.last_message_at = Some(Utc::now());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_channel_creation() {
        let channel = Channel::new("discord-main", ChannelType::Discord);
        assert_eq!(channel.name, "discord-main");
        assert_eq!(channel.channel_type, ChannelType::Discord);
        assert_eq!(channel.status, ChannelStatus::Disconnected);
    }

    #[test]
    fn test_channel_connect_disconnect() {
        let mut channel = Channel::new("test", ChannelType::WebSocket);
        channel.connect();
        assert_eq!(channel.status, ChannelStatus::Connected);
        channel.disconnect();
        assert_eq!(channel.status, ChannelStatus::Disconnected);
    }

    #[test]
    fn test_channel_record_message() {
        let mut channel = Channel::new("test", ChannelType::Telegram);
        channel.record_message();
        assert_eq!(channel.message_count, 1);
        assert!(channel.last_message_at.is_some());
    }
}
