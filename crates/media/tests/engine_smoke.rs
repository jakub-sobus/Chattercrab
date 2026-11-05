//! Starts the real engine, exercises the command surface, and shuts it down.
//! Works without audio devices: device failures arrive as `MediaEvent::Error`.

#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

use std::time::Duration;

use chattercrab_media::{MediaCommand, MediaConfig, MediaEngine, MediaEvent};
use chattercrab_protocol::PeerId;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn engine_starts_configures_and_shuts_down() {
    let mut engine = MediaEngine::start(MediaConfig::default()).unwrap();
    let me = PeerId::random();
    let cmds = [
        MediaCommand::Configure { me, ice: None },
        MediaCommand::SetMuted(true),
        MediaCommand::SetDeafened(true),
        MediaCommand::SetNoiseSuppression(true),
        MediaCommand::SetPeerVolume {
            peer: PeerId::random(),
            volume: 1.5,
        },
        MediaCommand::AddPeer {
            peer: PeerId::random(),
            initiate: false,
        },
        MediaCommand::StopScreenShare,
    ];
    for c in cmds {
        engine.commands.send(c).await.unwrap();
    }

    // Levels arrive periodically once configured.
    let saw_levels = tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(ev) = engine.events.recv().await {
            if matches!(ev, MediaEvent::Levels { .. }) {
                return true;
            }
        }
        false
    })
    .await
    .unwrap_or(false);
    assert!(saw_levels, "no Levels event");

    engine
        .commands
        .send(MediaCommand::RemoveAllPeers)
        .await
        .unwrap();
    engine.commands.send(MediaCommand::Shutdown).await.unwrap();
    // The event channel closes once the engine has stopped.
    let closed = tokio::time::timeout(Duration::from_secs(10), async {
        while engine.events.recv().await.is_some() {}
    })
    .await;
    assert!(closed.is_ok(), "engine did not shut down");
}

#[test]
fn start_outside_runtime_is_an_error() {
    assert!(MediaEngine::start(MediaConfig::default()).is_err());
}
