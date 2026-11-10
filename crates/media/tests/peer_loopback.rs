//! Two real `PeerManager`s connected over loopback with a fake signaling channel.
//!
//! Verifies that negotiation completes, Opus RTP flows, and that simultaneous screen-share
//! renegotiation from both sides (glare) is resolved by perfect negotiation. Also checks
//! that a connection is established when no media is written at all (a silent or missing
//! microphone), and that DTX frames written as empty samples leave RTP timestamp gaps
//! without sequence-number gaps.

#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use chattercrab_media::peer::{PeerEvent, PeerManager, PeerSettings, SendTargets};
use chattercrab_media::{OutgoingSignal, PeerConnectionState};
use chattercrab_protocol::PeerId;
use tokio::sync::watch;
use webrtc::media_stream::track_remote::{TrackRemote, TrackRemoteEvent};

fn settings() -> PeerSettings {
    PeerSettings {
        udp_addrs: vec!["127.0.0.1:0".into()],
        tcp_addrs: vec![],
        include_loopback: true,
    }
}

/// Writes `payload` to every target every 20 ms until the task is aborted.
fn pump(
    mut targets: watch::Receiver<SendTargets>,
    payload: &'static [u8],
    every: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(every);
        loop {
            tick.tick().await;
            let snapshot = targets.borrow_and_update().clone();
            for t in snapshot.iter() {
                let _ = t.write(Bytes::from_static(payload), every).await;
            }
        }
    })
}

fn count_rtp(track: Arc<dyn TrackRemote>, counter: Arc<AtomicUsize>) {
    record_rtp(track, counter, Arc::default());
}

/// Counts RTP packets and records their (sequence number, timestamp).
fn record_rtp(
    track: Arc<dyn TrackRemote>,
    counter: Arc<AtomicUsize>,
    headers: Arc<Mutex<Vec<(u16, u32)>>>,
) {
    tokio::spawn(async move {
        while let Some(ev) = track.poll().await {
            if let TrackRemoteEvent::OnRtpPacket(pkt) = ev {
                headers
                    .lock()
                    .unwrap()
                    .push((pkt.header.sequence_number, pkt.header.timestamp));
                counter.fetch_add(1, Ordering::Relaxed);
            }
        }
    });
}

struct Side {
    id: PeerId,
    pm: PeerManager,
    audio_rx: Arc<AtomicUsize>,
    audio_headers: Arc<Mutex<Vec<(u16, u32)>>>,
    video_rx: Arc<AtomicUsize>,
    video_tracks: usize,
    connected: bool,
}

/// Moves queued events from `from` into `to`.
async fn route(from: &mut Side, to: &mut Side) {
    let events: Vec<PeerEvent> = from.pm.drain_events().collect();
    for ev in events {
        match ev {
            PeerEvent::Signal(OutgoingSignal::Offer { sdp, .. }) => {
                to.pm.remote_offer(from.id, sdp).await.unwrap();
            }
            PeerEvent::Signal(OutgoingSignal::Answer { sdp, .. }) => {
                to.pm.remote_answer(from.id, sdp).await.unwrap();
            }
            PeerEvent::Signal(OutgoingSignal::Ice {
                candidate,
                sdp_mid,
                sdp_mline_index,
                ..
            }) => {
                to.pm
                    .remote_ice(from.id, candidate, sdp_mid, sdp_mline_index)
                    .await;
            }
            PeerEvent::RemoteAudio { track, .. } => {
                record_rtp(track, from.audio_rx.clone(), from.audio_headers.clone())
            }
            PeerEvent::RemoteVideo { track, .. } => {
                from.video_tracks += 1;
                count_rtp(track, from.video_rx.clone());
            }
            PeerEvent::State { state, .. } => {
                if state == PeerConnectionState::Connected {
                    from.connected = true;
                }
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn audio_flows_and_screen_glare_resolves() {
    let (pm_a, mut rx_a) = PeerManager::new(settings());
    let (pm_b, mut rx_b) = PeerManager::new(settings());
    let mut a = Side {
        id: PeerId::random(),
        pm: pm_a,
        audio_rx: Arc::default(),
        audio_headers: Arc::default(),
        video_rx: Arc::default(),
        video_tracks: 0,
        connected: false,
    };
    let mut b = Side {
        id: PeerId::random(),
        pm: pm_b,
        audio_rx: Arc::default(),
        audio_headers: Arc::default(),
        video_rx: Arc::default(),
        video_tracks: 0,
        connected: false,
    };
    a.pm.configure(a.id, None);
    b.pm.configure(b.id, None);

    // A is the newcomer: it initiates. B waits for the offer.
    b.pm.add_peer(a.id, false).await.unwrap();
    a.pm.add_peer(b.id, true).await.unwrap();

    // Opus DTX-style 3-byte payloads; H.264 payload is an Annex-B IDR slice stub.
    static OPUS: [u8; 3] = [0xF8, 0xFF, 0xFE];
    static H264: [u8; 9] = [0, 0, 0, 1, 0x65, 0x88, 0x84, 0x00, 0x33];
    let pumps = [
        pump(a.pm.audio_targets(), &OPUS, Duration::from_millis(20)),
        pump(b.pm.audio_targets(), &OPUS, Duration::from_millis(20)),
        pump(a.pm.video_targets(), &H264, Duration::from_millis(66)),
        pump(b.pm.video_targets(), &H264, Duration::from_millis(66)),
    ];

    let mut screen_started = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        tokio::select! {
            Some(ev) = rx_a.recv() => a.pm.handle_internal(ev).await,
            Some(ev) = rx_b.recv() => b.pm.handle_internal(ev).await,
            _ = tokio::time::sleep(Duration::from_millis(10)) => {}
        }
        route(&mut a, &mut b).await;
        route(&mut b, &mut a).await;

        let audio_ok =
            a.audio_rx.load(Ordering::Relaxed) >= 25 && b.audio_rx.load(Ordering::Relaxed) >= 25;
        if audio_ok && !screen_started {
            // Both sides start sharing at once: renegotiation glare.
            screen_started = true;
            a.pm.set_screen_active(true, false, 30).await;
            b.pm.set_screen_active(true, false, 30).await;
        }
        let video_ok =
            a.video_rx.load(Ordering::Relaxed) >= 5 && b.video_rx.load(Ordering::Relaxed) >= 5;
        if audio_ok && video_ok {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timeout: audio a={} b={}, video a={} b={} (tracks a={} b={})",
            a.audio_rx.load(Ordering::Relaxed),
            b.audio_rx.load(Ordering::Relaxed),
            a.video_rx.load(Ordering::Relaxed),
            b.video_rx.load(Ordering::Relaxed),
            a.video_tracks,
            b.video_tracks,
        );
    }
    for p in pumps {
        p.abort();
    }
    a.pm.remove_all().await;
    b.pm.remove_all().await;
}

/// Regression test: webrtc 0.21.1 only delivers `negotiationneeded` and starts ICE checks
/// when its driver wakes, and track/candidate changes alone do not wake it. Without the
/// engine's explicit wake, a peer whose microphone produces no frames never connected.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn connects_without_any_media_written() {
    let (pm_a, mut rx_a) = PeerManager::new(settings());
    let (pm_b, mut rx_b) = PeerManager::new(settings());
    let side = |pm| Side {
        id: PeerId::random(),
        pm,
        audio_rx: Arc::default(),
        audio_headers: Arc::default(),
        video_rx: Arc::default(),
        video_tracks: 0,
        connected: false,
    };
    let mut a = side(pm_a);
    let mut b = side(pm_b);
    a.pm.configure(a.id, None);
    b.pm.configure(b.id, None);
    b.pm.add_peer(a.id, false).await.unwrap();
    a.pm.add_peer(b.id, true).await.unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while !(a.connected && b.connected) {
        tokio::select! {
            Some(ev) = rx_a.recv() => a.pm.handle_internal(ev).await,
            Some(ev) = rx_b.recv() => b.pm.handle_internal(ev).await,
            _ = tokio::time::sleep(Duration::from_millis(10)) => {}
        }
        route(&mut a, &mut b).await;
        route(&mut b, &mut a).await;
        assert!(
            tokio::time::Instant::now() < deadline,
            "not connected without media: a={} b={}",
            a.connected,
            b.connected
        );
    }
    a.pm.remove_all().await;
    b.pm.remove_all().await;
}

/// The audio pump writes DTX frames as empty samples: nothing is sent, the RTP timestamp
/// still advances and the sequence numbers stay contiguous.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn empty_samples_leave_timestamp_gaps_without_sequence_gaps() {
    let (pm_a, mut rx_a) = PeerManager::new(settings());
    let (pm_b, mut rx_b) = PeerManager::new(settings());
    let side = |pm| Side {
        id: PeerId::random(),
        pm,
        audio_rx: Arc::default(),
        audio_headers: Arc::default(),
        video_rx: Arc::default(),
        video_tracks: 0,
        connected: false,
    };
    let mut a = side(pm_a);
    let mut b = side(pm_b);
    a.pm.configure(a.id, None);
    b.pm.configure(b.id, None);
    b.pm.add_peer(a.id, false).await.unwrap();
    a.pm.add_peer(b.id, true).await.unwrap();

    // Of every five 20 ms frames, two are sent and three are DTX (empty samples).
    static OPUS: [u8; 3] = [0xF8, 0xFF, 0xFE];
    let mut targets = a.pm.audio_targets();
    let pump = tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_millis(20));
        let mut frame = 0u64;
        loop {
            tick.tick().await;
            let data = if frame % 5 < 2 {
                Bytes::from_static(&OPUS)
            } else {
                Bytes::new()
            };
            let snapshot = targets.borrow_and_update().clone();
            for t in snapshot.iter() {
                t.write(data.clone(), Duration::from_millis(20))
                    .await
                    .unwrap_or_else(|e| assert!(!data.is_empty(), "empty sample failed: {e}"));
            }
            frame += 1;
        }
    });

    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while b.audio_rx.load(Ordering::Relaxed) < 40 {
        tokio::select! {
            Some(ev) = rx_a.recv() => a.pm.handle_internal(ev).await,
            Some(ev) = rx_b.recv() => b.pm.handle_internal(ev).await,
            _ = tokio::time::sleep(Duration::from_millis(10)) => {}
        }
        route(&mut a, &mut b).await;
        route(&mut b, &mut a).await;
        assert!(
            tokio::time::Instant::now() < deadline,
            "timeout: audio b={}",
            b.audio_rx.load(Ordering::Relaxed)
        );
    }
    pump.abort();

    // Look at the steady state only: the engine's driver wake-ups during negotiation
    // write extra frames.
    let headers = b.audio_headers.lock().unwrap().clone();
    let tail = &headers[headers.len() - 20..];
    let mut gaps = 0;
    for pair in tail.windows(2) {
        let (seq0, ts0) = pair[0];
        let (seq1, ts1) = pair[1];
        assert_eq!(seq1, seq0.wrapping_add(1), "sequence gap in {tail:?}");
        let dt = ts1.wrapping_sub(ts0);
        assert!(dt > 0 && dt % 960 == 0, "timestamp step {dt} in {tail:?}");
        if dt == 4 * 960 {
            gaps += 1;
        }
    }
    assert!(gaps >= 5, "expected DTX gaps in {tail:?}");
    a.pm.remove_all().await;
    b.pm.remove_all().await;
}
