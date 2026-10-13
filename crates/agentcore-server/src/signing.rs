//! Signatures on what agentcore distributes.
//!
//! Update manifests and release attestations are signed with an Ed25519 key
//! as [DSSE](https://github.com/secure-systems-lab/dsse) envelopes (the
//! format in-toto and Sigstore use): the signature covers the exact payload
//! bytes, so clients never have to re-serialise JSON to verify it. The
//! private key is encrypted with the master key in PostgreSQL; rotated keys
//! stay published so older signatures still verify. Webhooks carry an
//! HMAC-SHA256 of their body.

use agentcore_store::Store;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use ring::rand::SystemRandom;
use ring::signature::{ED25519, Ed25519KeyPair, KeyPair, UnparsedPublicKey};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// Payload type of an update manifest.
pub const MANIFEST_TYPE: &str = "application/vnd.agentcore.update+json";
/// Payload type of an in-toto statement.
pub const IN_TOTO_TYPE: &str = "application/vnd.in-toto+json";

/// DSSE pre-authentication encoding: what is actually signed.
pub fn pae(payload_type: &str, payload: &[u8]) -> Vec<u8> {
    let mut out = format!(
        "DSSEv1 {} {payload_type} {} ",
        payload_type.len(),
        payload.len()
    )
    .into_bytes();
    out.extend_from_slice(payload);
    out
}

/// Short id of a public key: the first 16 hex digits of its SHA-256.
pub fn key_id(public_key: &[u8]) -> String {
    hex::encode(Sha256::digest(public_key))[..16].to_string()
}

/// The active signing key, made on first use.
pub async fn active_key(store: &Store) -> anyhow::Result<(String, Ed25519KeyPair)> {
    if let Some((id, pkcs8)) = store.active_signing_key().await? {
        let pair = Ed25519KeyPair::from_pkcs8(&pkcs8)
            .map_err(|_| anyhow::anyhow!("the stored signing key is not valid"))?;
        return Ok((id, pair));
    }
    new_key(store, true, "system").await?;
    let (id, pkcs8) = store
        .active_signing_key()
        .await?
        .ok_or_else(|| anyhow::anyhow!("no signing key"))?;
    let pair = Ed25519KeyPair::from_pkcs8(&pkcs8)
        .map_err(|_| anyhow::anyhow!("the stored signing key is not valid"))?;
    Ok((id, pair))
}

/// Make a new key and make it the active one; returns its id.
pub async fn new_key(store: &Store, only_if_none: bool, actor: &str) -> anyhow::Result<String> {
    let rng = SystemRandom::new();
    let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng)
        .map_err(|_| anyhow::anyhow!("could not generate a signing key"))?;
    let pair = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref())
        .map_err(|_| anyhow::anyhow!("could not read the new signing key"))?;
    let public = pair.public_key().as_ref().to_vec();
    let id = key_id(&public);
    store
        .add_signing_key(&id, &public, pkcs8.as_ref(), only_if_none, actor)
        .await?;
    Ok(id)
}

/// Sign `payload` as a DSSE envelope.
pub fn envelope(key_id: &str, pair: &Ed25519KeyPair, payload_type: &str, payload: &[u8]) -> Value {
    let sig = pair.sign(&pae(payload_type, payload));
    json!({
        "payloadType": payload_type,
        "payload": B64.encode(payload),
        "signatures": [{ "keyid": key_id, "sig": B64.encode(sig.as_ref()) }],
    })
}

/// Sign a JSON document with the active key.
pub async fn sign_json(store: &Store, payload_type: &str, doc: &Value) -> anyhow::Result<Value> {
    let (id, pair) = active_key(store).await?;
    Ok(envelope(
        &id,
        &pair,
        payload_type,
        serde_json::to_vec(doc)?.as_slice(),
    ))
}

/// Check an envelope against published keys (`(key_id, public key)`) and
/// return its payload. What update clients do; used by tests and the CLI.
pub fn verify(envelope: &Value, keys: &[(String, Vec<u8>)]) -> Result<Value, String> {
    let payload_type = envelope["payloadType"]
        .as_str()
        .ok_or("the envelope has no payloadType")?;
    let payload = B64
        .decode(
            envelope["payload"]
                .as_str()
                .ok_or("the envelope has no payload")?,
        )
        .map_err(|_| "the payload is not base64")?;
    let message = pae(payload_type, &payload);
    let signatures = envelope["signatures"]
        .as_array()
        .ok_or("the envelope has no signatures")?;
    for s in signatures {
        let (Some(keyid), Some(sig)) = (s["keyid"].as_str(), s["sig"].as_str()) else {
            continue;
        };
        let Some((_, public)) = keys.iter().find(|(id, _)| id == keyid) else {
            continue;
        };
        let Ok(sig) = B64.decode(sig) else { continue };
        if UnparsedPublicKey::new(&ED25519, public)
            .verify(&message, &sig)
            .is_ok()
        {
            return serde_json::from_slice(&payload)
                .map_err(|_| "the payload is not JSON".to_string());
        }
    }
    Err("no valid signature from a known key".into())
}

/// The published keys, for clients.
pub fn published(keys: &[agentcore_store::SigningKey]) -> Value {
    json!({
        "keys": keys.iter().map(|k| json!({
            "key_id": k.key_id,
            "algorithm": "ed25519",
            "public_key": B64.encode(&k.public_key),
            "created_at": k.created_at,
            "retired_at": k.retired_at,
        })).collect::<Vec<_>>(),
    })
}

/// `sha256=<hex>`: HMAC-SHA256 of a webhook body.
pub fn hmac_signature(secret: &str, body: &[u8]) -> String {
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, secret.as_bytes());
    format!(
        "sha256={}",
        hex::encode(ring::hmac::sign(&key, body).as_ref())
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair() -> (String, Ed25519KeyPair, Vec<u8>) {
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).unwrap();
        let pair = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
        let public = pair.public_key().as_ref().to_vec();
        (key_id(&public), pair, public)
    }

    #[test]
    fn envelopes_verify_and_detect_tampering() {
        let (id, pair, public) = pair();
        let doc = json!({ "version": "1.2.3", "sha256": "ab" });
        let env = envelope(
            &id,
            &pair,
            MANIFEST_TYPE,
            &serde_json::to_vec(&doc).unwrap(),
        );
        let keys = vec![(id.clone(), public)];
        assert_eq!(verify(&env, &keys).unwrap(), doc);

        let mut tampered = env.clone();
        tampered["payload"] = json!(B64.encode(br#"{"version":"6.6.6"}"#));
        assert!(verify(&tampered, &keys).is_err());
        // The payload type is signed too.
        let mut retyped = env.clone();
        retyped["payloadType"] = json!(IN_TOTO_TYPE);
        assert!(verify(&retyped, &keys).is_err());
        // Unknown keys are not trusted.
        let (_, _, other) = self::pair();
        assert!(verify(&env, &[(id, other)]).is_err());
    }

    #[test]
    fn pae_matches_the_dsse_spec() {
        assert_eq!(
            pae("http://example.com/HelloWorld", b"hello world"),
            b"DSSEv1 29 http://example.com/HelloWorld 11 hello world"
        );
    }

    #[test]
    fn webhook_signatures_are_hmac_sha256() {
        // RFC 4231 test case 2.
        assert_eq!(
            hmac_signature("Jefe", b"what do ya want for nothing?"),
            "sha256=5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }
}
