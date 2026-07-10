//! The host → client notification path (spec 05): the agent pushes an
//! `A2C::Notification` and the client surfaces it as a `ControlEvent`.
//! Controllers can't run on CI, but the notification plumbing (control-stream
//! reader → embedder channel) can — this is that coverage. The loopback
//! handshake lives in [`common::MockAgent`].

mod common;

use std::time::Duration;

use common::MockAgent;
use gsa_client_core::{Client, ControlEvent, ServerAuth};
use gsa_core::media::H264Profile;
use gsa_protocol::control::{A2C, Notification};

#[tokio::test]
async fn gamepad_connected_notification_reaches_the_client() {
    let agent = MockAgent::start().await;

    let mut client = Client::connect(agent.addr, "test", H264Profile::High, ServerAuth::Open)
        .await
        .expect("connect");
    let mut events = client
        .take_control_events()
        .expect("control events available");

    agent.push(A2C::Notification(Notification::GamepadConnected {
        seat: 1,
    }));

    let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
        .await
        .expect("notification within timeout")
        .expect("channel stayed open");
    assert!(matches!(event, ControlEvent::GamepadConnected { seat: 1 }));

    client.close().await;
}

#[tokio::test]
async fn gamepad_disconnected_notification_reaches_the_client() {
    let agent = MockAgent::start().await;

    let mut client = Client::connect(agent.addr, "test", H264Profile::High, ServerAuth::Open)
        .await
        .expect("connect");
    let mut events = client
        .take_control_events()
        .expect("control events available");

    agent.push(A2C::Notification(Notification::GamepadDisconnected {
        seat: 0,
    }));

    let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
        .await
        .expect("notification within timeout")
        .expect("channel stayed open");
    assert!(matches!(
        event,
        ControlEvent::GamepadDisconnected { seat: 0 }
    ));

    client.close().await;
}
