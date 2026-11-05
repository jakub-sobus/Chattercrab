//! Two-process end-to-end test: a real signaling server plus separate `signal_call`
//! processes, each running the real `MediaEngine` with a synthetic tone, talking WebRTC
//! to each other over localhost (no TURN).
//!
//! Ignored by default because it spawns processes and needs real UDP sockets:
//!
//! ```text
//! cargo test -p chattercrab-media --test two_process -- --ignored --nocapture
//! ```
//!
//! The test builds the `signal_call` example with the same profile first.
//! `CHATTERCRAB_E2E_PEERS` (default 2) sets the number of participants and
//! `CHATTERCRAB_E2E_SECONDS` (default 10) the call length.

#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use chattercrab_protocol::{CreateRoomRequest, RoomName, RoomSummary};
use chattercrab_server::{Config, ServerSignals, run};

fn env_or(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Builds the `signal_call` example and returns its path, next to the test binary's
/// profile directory (`target/<profile>/examples/`).
fn build_example() -> PathBuf {
    let test_exe = std::env::current_exe().unwrap();
    // target/<profile>/deps/two_process-<hash>
    let profile_dir = test_exe.parent().unwrap().parent().unwrap().to_path_buf();
    let release = profile_dir.file_name().is_some_and(|n| n == "release");
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let mut cmd = Command::new(cargo);
    cmd.args([
        "build",
        "-q",
        "-p",
        "chattercrab-media",
        "--example",
        "signal_call",
    ]);
    if release {
        cmd.arg("--release");
    }
    let status = cmd.status().expect("run cargo build");
    assert!(status.success(), "building the signal_call example failed");
    let exe = profile_dir
        .join("examples")
        .join(format!("signal_call{}", std::env::consts::EXE_SUFFIX));
    assert!(exe.exists(), "example not found at {}", exe.display());
    exe
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "spawns processes and needs real UDP sockets; run with --ignored"]
async fn peers_hear_each_other_through_real_server() {
    let peers = env_or("CHATTERCRAB_E2E_PEERS", 2) as usize;
    let seconds = env_or("CHATTERCRAB_E2E_SECONDS", 10);
    assert!(peers >= 2, "need at least two peers");
    let exe = tokio::task::spawn_blocking(build_example).await.unwrap();

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let config = Config {
        bind: addr,
        ..Config::default()
    };
    let signals = ServerSignals::default();
    let server = tokio::spawn(run(config, listener, signals.clone()));

    let _ = rustls::crypto::ring::default_provider().install_default();
    let base = format!("http://{addr}");
    let room: RoomSummary = reqwest::Client::new()
        .post(format!("{base}/rooms"))
        .json(&CreateRoomRequest {
            name: RoomName::new("e2e").unwrap(),
        })
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();

    // Staggered joins exercise both the newcomer (offering) and the existing-member
    // (answering) paths. Everyone stays until the last one has had `seconds`.
    let tones = [440.0, 660.0, 880.0, 550.0, 770.0];
    let mut children = Vec::new();
    for i in 0..peers {
        let stay = seconds + (peers - 1 - i) as u64;
        let child = Command::new(&exe)
            .args([
                "--server",
                &base,
                "--room",
                room.id.as_str(),
                "--name",
                &format!("peer{i}"),
                "--tone",
                &tones[i % tones.len()].to_string(),
                "--seconds",
                &stay.to_string(),
                "--expect-peers",
                &(peers - 1).to_string(),
                // Every participant leaves at the same moment, so each pair overlaps for
                // at least `seconds`; allow generous time for connection setup.
                "--min-heard-secs",
                &(seconds as f32 / 2.0).to_string(),
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn signal_call");
        children.push(child);
        tokio::time::sleep(Duration::from_secs(1)).await;
    }

    let outputs = tokio::task::spawn_blocking(move || {
        children
            .into_iter()
            .map(|c| c.wait_with_output().expect("wait for signal_call"))
            .collect::<Vec<_>>()
    })
    .await
    .unwrap();

    signals.shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(5), server).await;

    let mut all_ok = true;
    for out in &outputs {
        let stdout = String::from_utf8_lossy(&out.stdout);
        let result = stdout
            .lines()
            .find(|l| l.starts_with("RESULT"))
            .unwrap_or("RESULT missing");
        println!("{result}");
        all_ok &= out.status.success() && result.contains("ok=true");
    }
    assert!(
        all_ok,
        "at least one participant did not hear every other peer"
    );
}
