//! §4.0 Phase 1: the registry resolves a channel id to the gateway registered
//! under it, and refuses to silently replace one.

use ardur_messaging_gateway::{ChannelId, GatewayRegistry, InProcessGateway, RegistryError};

#[test]
fn lookup_returns_the_gateway_registered_for_each_channel() {
    let first = ChannelId("in-process://first".to_owned());
    let second = ChannelId("in-process://second".to_owned());

    let mut registry = GatewayRegistry::new();
    registry
        .register(Box::new(InProcessGateway::new(first.clone())))
        .expect("first registers");
    registry
        .register(Box::new(InProcessGateway::new(second.clone())))
        .expect("second registers");

    assert_eq!(
        registry.get(&first).expect("first present").channel_id(),
        first
    );
    assert_eq!(
        registry.get(&second).expect("second present").channel_id(),
        second
    );
    assert!(
        registry
            .get(&ChannelId("in-process://absent".to_owned()))
            .is_none()
    );
}

#[test]
fn registering_a_duplicate_channel_is_rejected() {
    let channel = ChannelId("in-process://dup".to_owned());
    let mut registry = GatewayRegistry::new();
    registry
        .register(Box::new(InProcessGateway::new(channel.clone())))
        .expect("first registers");

    let err = registry
        .register(Box::new(InProcessGateway::new(channel.clone())))
        .expect_err("duplicate is rejected");
    assert!(matches!(err, RegistryError::AlreadyRegistered(id) if id == channel));
}
