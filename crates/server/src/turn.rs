//! TURN REST API credentials (draft-uberti-behave-turn-rest).

use base64::Engine;
use chattercrab_protocol::TurnCredentials;
use hmac::{Hmac, KeyInit, Mac};
use sha1::Sha1;

use crate::config::TurnConfig;

/// Lifetime of issued TURN credentials: 12 hours.
pub const TURN_TTL_SECS: u64 = 12 * 60 * 60;

/// The TURN secret could not be used as an HMAC key.
#[derive(Debug, thiserror::Error)]
#[error("invalid TURN secret")]
pub struct InvalidSecret;

/// Computes `base64(HMAC-SHA1(secret, username))`.
pub fn credential_for(secret: &str, username: &str) -> Result<String, InvalidSecret> {
    let mut mac = Hmac::<Sha1>::new_from_slice(secret.as_bytes()).map_err(|_| InvalidSecret)?;
    mac.update(username.as_bytes());
    let tag = mac.finalize().into_bytes();
    Ok(base64::engine::general_purpose::STANDARD.encode(tag))
}

/// Issues credentials valid until `now_unix + TURN_TTL_SECS`.
pub fn issue(turn: &TurnConfig, now_unix: u64) -> Result<TurnCredentials, InvalidSecret> {
    let expiry = now_unix + TURN_TTL_SECS;
    let mut nonce = [0u8; 8];
    rand::fill(&mut nonce);
    let random: String = nonce.iter().map(|b| format!("{b:02x}")).collect();
    let username = format!("{expiry}:{random}");
    let credential = credential_for(&turn.secret, &username)?;
    Ok(TurnCredentials {
        username,
        credential,
        ttl_secs: TURN_TTL_SECS,
        urls: with_stun_urls(&turn.urls),
    })
}

/// Returns `urls` preceded by a `stun:host:port` entry for every `turn:` URL that
/// uses UDP (explicitly or by default). Duplicates are removed.
pub fn with_stun_urls(urls: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(urls.len() + 1);
    for url in urls {
        let Some(rest) = url.strip_prefix("turn:") else {
            continue;
        };
        let (host_port, query) = rest.split_once('?').unwrap_or((rest, ""));
        let udp = query.is_empty() || query.split('&').any(|kv| kv == "transport=udp");
        if udp && !host_port.is_empty() {
            let stun = format!("stun:{host_port}");
            if !out.contains(&stun) {
                out.push(stun);
            }
        }
    }
    for url in urls {
        if !out.contains(url) {
            out.push(url.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_vector() {
        // Reference computed with Python's hmac module.
        assert_eq!(
            credential_for("north-sea-crab", "1793918400:9f2c4e7a1b3d5f60").unwrap(),
            "JJ6+w6rZCy41aFa682aUle+dzcY="
        );
    }

    #[test]
    fn issue_shape() {
        let turn = TurnConfig {
            secret: "s".into(),
            realm: "r".into(),
            urls: vec!["turn:h:3478?transport=udp".into()],
        };
        let c = issue(&turn, 1000).unwrap();
        let (expiry, random) = c.username.split_once(':').unwrap();
        assert_eq!(expiry, "44200");
        assert_eq!(random.len(), 16);
        assert_eq!(c.ttl_secs, 43_200);
        assert_eq!(c.credential, credential_for("s", &c.username).unwrap());
        assert_eq!(c.urls, ["stun:h:3478", "turn:h:3478?transport=udp"]);
    }

    #[test]
    fn stun_derivation() {
        let urls: Vec<String> = [
            "turn:chat.example.com:3478?transport=udp",
            "turn:chat.example.com:3478?transport=tcp",
            "turns:chat.example.com:5349?transport=tcp",
            "turn:other:3478",
        ]
        .map(String::from)
        .to_vec();
        assert_eq!(
            with_stun_urls(&urls),
            [
                "stun:chat.example.com:3478",
                "stun:other:3478",
                "turn:chat.example.com:3478?transport=udp",
                "turn:chat.example.com:3478?transport=tcp",
                "turns:chat.example.com:5349?transport=tcp",
                "turn:other:3478",
            ]
        );
        let tcp_only = vec!["turn:h:3478?transport=tcp".to_string()];
        assert_eq!(with_stun_urls(&tcp_only), tcp_only);
    }
}
