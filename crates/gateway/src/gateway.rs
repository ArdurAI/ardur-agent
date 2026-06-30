use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::channel::{Channel, ChannelId, ChannelStatus};
use crate::message::{Message, MessageId, MessageStatus};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum GatewayStatus {
    Initializing,
    Running,
    Paused,
    ShuttingDown,
    Stopped,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayConfig {
    pub name: String,
    pub max_channels: usize,
    pub message_buffer_size: usize,
    pub enable_routing: bool,
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            name: "ardur-gateway".to_string(),
            max_channels: 100,
            message_buffer_size: 1000,
            enable_routing: true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Gateway {
    config: GatewayConfig,
    status: std::sync::Arc<std::sync::RwLock<GatewayStatus>>,
    channels: std::sync::Arc<std::sync::RwLock<HashMap<ChannelId, Channel>>>,
    messages: std::sync::Arc<std::sync::RwLock<HashMap<MessageId, Message>>>,
    started_at: DateTime<Utc>,
}

impl Gateway {
    pub fn new(config: GatewayConfig) -> Self {
        Self {
            config,
            status: std::sync::Arc::new(std::sync::RwLock::new(GatewayStatus::Initializing)),
            channels: std::sync::Arc::new(std::sync::RwLock::new(HashMap::new())),
            messages: std::sync::Arc::new(std::sync::RwLock::new(HashMap::new())),
            started_at: Utc::now(),
        }
    }

    pub fn start(&self) -> crate::error::Result<()> {
        let mut status = self.status.write().map_err(|_| {
            crate::error::GatewayError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "poisoned lock",
            ))
        })?;
        *status = GatewayStatus::Running;
        Ok(())
    }

    pub fn pause(&self) -> crate::error::Result<()> {
        let mut status = self.status.write().map_err(|_| {
            crate::error::GatewayError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "poisoned lock",
            ))
        })?;
        *status = GatewayStatus::Paused;
        Ok(())
    }

    pub fn shutdown(&self) -> crate::error::Result<()> {
        let mut status = self.status.write().map_err(|_| {
            crate::error::GatewayError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "poisoned lock",
            ))
        })?;
        *status = GatewayStatus::ShuttingDown;
        Ok(())
    }

    pub fn status(&self) -> crate::error::Result<GatewayStatus> {
        let status = self.status.read().map_err(|_| {
            crate::error::GatewayError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "poisoned lock",
            ))
        })?;
        Ok(status.clone())
    }

    pub fn add_channel(&self, channel: Channel) -> crate::error::Result<ChannelId> {
        let mut channels = self.channels.write().map_err(|_| {
            crate::error::GatewayError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "poisoned lock",
            ))
        })?;
        let id = channel.id.clone();
        channels.insert(id.clone(), channel);
        Ok(id)
    }

    pub fn get_channel(&self, id: &ChannelId) -> crate::error::Result<Channel> {
        let channels = self.channels.read().map_err(|_| {
            crate::error::GatewayError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "poisoned lock",
            ))
        })?;
        channels
            .get(id)
            .cloned()
            .ok_or_else(|| crate::error::GatewayError::ChannelNotFound(id.clone()))
    }

    pub fn list_channels(&self) -> crate::error::Result<Vec<Channel>> {
        let channels = self.channels.read().map_err(|_| {
            crate::error::GatewayError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "poisoned lock",
            ))
        })?;
        Ok(channels.values().cloned().collect())
    }

    pub fn list_channels_by_status(
        &self,
        status: ChannelStatus,
    ) -> crate::error::Result<Vec<Channel>> {
        let channels = self.channels.read().map_err(|_| {
            crate::error::GatewayError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "poisoned lock",
            ))
        })?;
        Ok(channels
            .values()
            .filter(|c| c.status == status)
            .cloned()
            .collect())
    }

    pub fn send_message(&self, message: Message) -> crate::error::Result<MessageId> {
        let mut messages = self.messages.write().map_err(|_| {
            crate::error::GatewayError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "poisoned lock",
            ))
        })?;
        let id = message.id.clone();
        messages.insert(id.clone(), message);
        Ok(id)
    }

    pub fn get_message(&self, id: &MessageId) -> crate::error::Result<Message> {
        let messages = self.messages.read().map_err(|_| {
            crate::error::GatewayError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "poisoned lock",
            ))
        })?;
        messages
            .get(id)
            .cloned()
            .ok_or_else(|| crate::error::GatewayError::MessageNotFound(id.clone()))
    }

    pub fn message_count(&self) -> crate::error::Result<usize> {
        let messages = self.messages.read().map_err(|_| {
            crate::error::GatewayError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "poisoned lock",
            ))
        })?;
        Ok(messages.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel::ChannelType;

    #[test]
    fn test_gateway_creation() {
        let gateway = Gateway::new(GatewayConfig::default());
        assert_eq!(gateway.status().unwrap(), GatewayStatus::Initializing);
    }

    #[test]
    fn test_gateway_start_pause_shutdown() {
        let gateway = Gateway::new(GatewayConfig::default());
        gateway.start().unwrap();
        assert_eq!(gateway.status().unwrap(), GatewayStatus::Running);
        gateway.pause().unwrap();
        assert_eq!(gateway.status().unwrap(), GatewayStatus::Paused);
        gateway.shutdown().unwrap();
        assert_eq!(gateway.status().unwrap(), GatewayStatus::ShuttingDown);
    }

    #[test]
    fn test_gateway_add_and_get_channel() {
        let gateway = Gateway::new(GatewayConfig::default());
        let channel = Channel::new("test", ChannelType::Discord);
        let id = gateway.add_channel(channel.clone()).unwrap();
        let retrieved = gateway.get_channel(&id).unwrap();
        assert_eq!(retrieved.name, "test");
    }

    #[test]
    fn test_gateway_list_channels_by_status() {
        let gateway = Gateway::new(GatewayConfig::default());
        let mut ch1 = Channel::new("ch1", ChannelType::Discord);
        ch1.connect();
        let ch2 = Channel::new("ch2", ChannelType::Telegram);
        gateway.add_channel(ch1).unwrap();
        gateway.add_channel(ch2).unwrap();

        let connected = gateway
            .list_channels_by_status(ChannelStatus::Connected)
            .unwrap();
        assert_eq!(connected.len(), 1);
        assert_eq!(connected[0].name, "ch1");
    }

    #[test]
    fn test_gateway_send_and_get_message() {
        let gateway = Gateway::new(GatewayConfig::default());
        let message = Message::new("ch-1", "user1", "Hello", crate::message::MessageType::Text);
        let id = gateway.send_message(message.clone()).unwrap();
        let retrieved = gateway.get_message(&id).unwrap();
        assert_eq!(retrieved.content, "Hello");
    }

    #[test]
    fn test_gateway_message_count() {
        let gateway = Gateway::new(GatewayConfig::default());
        assert_eq!(gateway.message_count().unwrap(), 0);
        gateway
            .send_message(Message::new(
                "ch-1",
                "user1",
                "Hello",
                crate::message::MessageType::Text,
            ))
            .unwrap();
        assert_eq!(gateway.message_count().unwrap(), 1);
    }
}
