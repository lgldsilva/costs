//! Z.ai reset inventory (read-only enrichment): client signing handshake and
//! `customer-package-reset/list`. Read-only: `/use` redemption is intentionally
//! NOT here.
//!
//! Hygiene: plaintext buffers and derived HKDF keys are zeroized in drop scopes;
//! key/nonce/signature values are never logged; error paths degrade silently so
//! enrichment can never fail the quota fetch.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use ring::aead::{self, Aad, LessSafeKey, Nonce, UnboundKey};
use ring::digest::{SHA256, digest};
use ring::hkdf::{HKDF_SHA256, Salt};
use ring::rand::{SecureRandom, SystemRandom};
use ring::signature::Ed25519KeyPair;
use serde_json::Value;
use zeroize::Zeroize;

use super::creds::Ctx;
use super::entry::{ResetCredits, ResetExpiryGroup};

const HANDSHAKE_URL: &str = "https://api.z.ai/api/paas/c1f3a7e2/v2/client";
const LIST_URL: &str = "https://api.z.ai/api/biz/customer-package-reset/list?targetType=PERSONAL";
const KDF_SALT: &[u8] = b"WD_CLIENT_SIGN_KDF_SALT";
const INFO_SIGN_KEY: &[u8] = b"getSignKey_hmac";
const INFO_ED25519: &[u8] = b"ed25519_priv";
const CLIENT_VERSION: &str = "0.0.0-dev";
const APP_ID: &str = "zcode";

/// Split `id.secret`; the HKDF input is the secret AFTER the first dot only.
fn split_key(key: &str) -> Option<(&str, &str)> {
    let (id, secret) = key.split_once('.')?;
    (!id.is_empty() && !secret.is_empty()).then_some((id, secret))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// HKDF-SHA256(ikm=secret, salt=WD_CLIENT_SIGN_KDF_SALT, info, 32).
fn hkdf32(secret: &[u8], info: &[u8]) -> SecretBytes {
    let prk = Salt::new(HKDF_SHA256, KDF_SALT).extract(secret);
    let mut out = SecretBytes::new(vec![0u8; 32]);
    // fixed 32-byte output for a SHA-256 PRK; valid length is guaranteed here.
    prk.expand(&[info], HKDF_SHA256)
        .expect("32-byte HKDF-SHA256 output is valid")
        .fill(&mut out.0)
        .expect("32-byte HKDF-SHA256 output is valid");
    out
}

/// base64(HMAC-SHA256(HKDF("getSignKey_hmac"), "get_sign_key\n{id}\n{ts}\n{nonce}")).
fn handshake_sig(secret: &[u8], api_id: &str, ts: &str, nonce: &str) -> String {
    let key_bytes = hkdf32(secret, INFO_SIGN_KEY);
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, key_bytes.as_slice());
    let msg = format!("get_sign_key\n{api_id}\n{ts}\n{nonce}").into_bytes();
    let tag = ring::hmac::sign(&key, &msg);
    b64_encode(tag.as_ref())
}

/// AES-256-GCM decrypt of `iv(12) || ciphertext+tag(16)`; AAD = api key id;
/// plaintext = base64(PKCS#8 Ed25519 private key).
fn decrypt_pkcs8(aes_key: &[u8; 32], api_id: &str, private_cipher_b64: &str) -> Option<SecretBytes> {
    let mut blob = SecretBytes::new(b64_decode(private_cipher_b64)?);
    (|| {
        if blob.as_slice().len() < 12 + 16 {
            return None;
        }
        let unbound = UnboundKey::new(&aead::AES_256_GCM, aes_key).ok()?;
        let key = LessSafeKey::new(unbound);
        let nonce = Nonce::try_assume_unique_for_key(&blob.as_slice()[..12]).ok()?;
        let ct = &mut blob.0[12..];
        let plain = key.open_in_place(nonce, Aad::from(api_id.as_bytes()), ct).ok()?;
        let plain_len = plain.len();
        // Move plaintext to the start of the same allocation. `copy_within`
        // does not allocate; the duplicate tail is erased before returning.
        blob.0.copy_within(12..12 + plain_len, 0);
        blob.0[plain_len..].zeroize();
        blob.0.truncate(plain_len);
        Some(blob)
    })()
}

/// PoW seed: first 32 hex chars of sha256("{api_id}\nzcode\n{sid}\n{ts}").
fn pow_seed_hex(api_id: &str, session_id: &str, ts: &str) -> String {
    let pre = format!("{api_id}\n{APP_ID}\n{session_id}\n{ts}");
    hex(&digest(&SHA256, pre.as_bytes()).as_ref()[..16])
}

/// Find `a = start + i.toHex(8, padded)` with sha256("{seed}\n{a}")[0] == 0.
fn solve_pow(seed_hex: &str, start: &str) -> Option<String> {
    if seed_hex.len() != 32
        || !seed_hex.bytes().all(|b| b.is_ascii_hexdigit())
        || start.len() != 24
        || !start.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return None;
    }
    let mut buf = [0u8; 33];
    buf[..32].copy_from_slice(seed_hex.as_bytes());
    buf[32] = b'\n';
    for i in 0u64.. {
        let a = format!("{start}{i:08x}");
        let mut msg = Vec::with_capacity(41);
        msg.extend_from_slice(&buf);
        msg.extend_from_slice(a.as_bytes());
        let d = digest(&SHA256, &msg);
        if d.as_ref()[0] == 0 {
            return Some(a);
        }
    }
    None
}

fn sign_b64(kp: &Ed25519KeyPair, msg: &str) -> String {
    b64_encode(kp.sign(msg.as_bytes()).as_ref())
}

fn parse_credits(v: Option<&Value>) -> Option<ResetCredits> {
    let items = v?.as_array()?;
    let mut expires = Vec::new();
    let mut groups: HashMap<String, i64> = HashMap::new();
    let mut available = 0i64;
    for it in items {
        if it.get("available").and_then(Value::as_bool) != Some(true) {
            continue;
        }
        available += 1;
        if let Some(raw) = it.get("expireTime").and_then(Value::as_str) {
            *groups.entry(raw.to_owned()).or_default() += 1;
        }
    }
    expires.sort_unstable();
    let mut expiry_groups: Vec<ResetExpiryGroup> = groups
        .into_iter()
        .map(|(expires_at_raw, available)| ResetExpiryGroup {
            available,
            expires_at_raw,
        })
        .collect();
    expiry_groups.sort_by(|a, b| a.expires_at_raw.cmp(&b.expires_at_raw));
    Some(ResetCredits {
        available,
        expires,
        expiry_groups: Some(expiry_groups),
    })
}

/// `(five_hour, week)` credits from the list response; `None` when malformed.
pub fn parse_list(v: &Value) -> Option<(ResetCredits, ResetCredits)> {
    let data = v.get("data")?;
    Some((
        parse_credits(data.get("fiveHourResets"))?,
        parse_credits(data.get("weekResets"))?,
    ))
}

// --- base64 (standard alphabet, padded): tiny pure helpers, no extra dep ---

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn b64_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(B64[(n >> 18) as usize & 63] as char);
        out.push(B64[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            B64[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            B64[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

fn valid_b64_padding(s: &str, padding: usize) -> bool {
    if padding > 2 || s[..s.len() - padding].contains('=') {
        return false;
    }
    let unpadded_len = s.len() - padding;
    let valid_padding = match padding {
        0 => true,
        1 => unpadded_len % 4 == 3,
        2 => unpadded_len % 4 == 2,
        _ => false,
    };
    unpadded_len % 4 != 1 && (padding == 0 || (s.len().is_multiple_of(4) && valid_padding))
}

fn valid_b64_tail(s: &str) -> Option<()> {
    let Some(last) = s.as_bytes().last() else {
        return Some(());
    };
    let value = b64_value(*last)?;
    let unused_bits = match s.len() % 4 {
        2 => 4,
        3 => 2,
        _ => 0,
    };
    if unused_bits > 0 && value & ((1 << unused_bits) - 1) != 0 {
        return None;
    }
    Some(())
}

fn decode_b64_bytes(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    for c in s.bytes() {
        let v = b64_value(c)? as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

fn b64_decode(encoded: &str) -> Option<Vec<u8>> {
    let padding = encoded.len() - encoded.trim_end_matches('=').len();
    if !valid_b64_padding(encoded, padding) {
        return None;
    }
    let unpadded = &encoded[..encoded.len() - padding];
    valid_b64_tail(unpadded)?;
    decode_b64_bytes(unpadded)
}

fn b64_value(c: u8) -> Option<u8> {
    match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

// --- impure: network + randomness + in-process key cache ---

fn random_hex(n_bytes: usize) -> Option<String> {
    let mut buf = vec![0u8; n_bytes];
    SystemRandom::new().fill(&mut buf).ok()?;
    Some(hex(&buf))
}

fn now_ms() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis().to_string())
        .unwrap_or_default()
}

struct SecretBytes(Vec<u8>);

impl Drop for SecretBytes {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl SecretBytes {
    fn new(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }

    fn as_slice(&self) -> &[u8] {
        &self.0
    }
}

fn keypair_cache() -> &'static Mutex<HashMap<String, Arc<Ed25519KeyPair>>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Arc<Ed25519KeyPair>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Handshake + AES-GCM decrypt → Ed25519 keypair, cached per api key (map key
/// is the sha256 of the key so the raw secret is never retained by the cache).
const KEYPAIR_CACHE_CAPACITY: usize = 32;

fn insert_cached_keypair(cache: &mut HashMap<String, Arc<Ed25519KeyPair>>, cache_key: String, kp: Arc<Ed25519KeyPair>) {
    if cache.len() >= KEYPAIR_CACHE_CAPACITY
        && !cache.contains_key(&cache_key)
        && let Some(evicted) = cache.keys().next().cloned()
    {
        cache.remove(&evicted);
    }
    cache.insert(cache_key, kp);
}

fn obtain_keypair(ctx: &Ctx, key: &str, api_id: &str, secret: &[u8]) -> Option<Arc<Ed25519KeyPair>> {
    let cache_key = hex(digest(&SHA256, key.as_bytes()).as_ref());
    if let Ok(map) = keypair_cache().lock()
        && let Some(kp) = map.get(&cache_key)
    {
        return Some(kp.clone());
    }
    let ts = now_ms();
    let nonce = random_hex(16)?;
    let sig = handshake_sig(secret, api_id, &ts, &nonce);
    let body = format!(
        r#"{{"apiKey":{key},"nonce":"{nonce}","sig":"{sig}","ts":"{ts}"}}"#,
        key = serde_json::json!(key)
    );
    let hs = ctx
        .post(
            HANDSHAKE_URL,
            &[
                ("Authorization".to_string(), key.to_string()),
                ("Content-Type".to_string(), "application/json".to_string()),
            ],
            &body,
        )
        .ok()?;
    // This is encrypted transport material, not plaintext key material.
    let cipher = serde_json::from_str::<Value>(&hs)
        .ok()?
        .get("data")?
        .get("privateCipher")?
        .as_str()?
        .to_owned();
    let kp_result = (|| {
        let aes_key = hkdf32(secret, INFO_ED25519);
        let mut plain = decrypt_pkcs8(aes_key.as_slice().try_into().ok()?, api_id, &cipher)?;
        // Validate UTF-8 while keeping bytes in the zeroizing buffer. The b64
        // alphabet is ASCII, so bytes can be decoded directly after validation.
        let plain_b64 = std::str::from_utf8(plain.as_slice()).ok()?;
        let pkcs8 = SecretBytes::new(b64_decode(plain_b64)?);
        plain.0.zeroize();
        Ed25519KeyPair::from_pkcs8_maybe_unchecked(pkcs8.as_slice()).ok()
    })();
    let kp = Arc::new(kp_result?);
    if let Ok(mut map) = keypair_cache().lock() {
        insert_cached_keypair(&mut map, cache_key, kp.clone());
    }
    Some(kp)
}

/// Enrichment entry point: `(five_hour, week)` reset credits, or `None` on any
/// failure (caller must treat that as "no resets", never as a quota error).
pub fn enrich(ctx: &Ctx, key: &str) -> Option<(ResetCredits, ResetCredits)> {
    let (api_id, secret) = split_key(key)?;
    let secret = secret.as_bytes();
    let kp = obtain_keypair(ctx, key, api_id, secret)?;

    let session_id = random_hex(16)?;
    let ts = now_ms();
    let nonce = random_hex(16)?;
    let seed = pow_seed_hex(api_id, &session_id, &ts);
    let start = random_hex(12)?;
    let pow = solve_pow(&seed, &start)?;
    let msg = format!("{api_id}\n{ts}\n{CLIENT_VERSION}\n{session_id}\n{nonce}");
    let sig = sign_b64(&kp, &msg);
    let headers = vec![
        ("Authorization".to_string(), key.to_string()),
        ("X-Client-Ts".to_string(), ts),
        ("X-Client-Version".to_string(), CLIENT_VERSION.to_string()),
        ("X-Client-Sig".to_string(), sig),
        ("X-Session-Id".to_string(), session_id),
        ("X-Client-Nonce".to_string(), nonce),
        ("X-App-Id".to_string(), APP_ID.to_string()),
        ("X-Client-Pow".to_string(), pow),
    ];
    let body = ctx.get(LIST_URL, &headers).ok()?;
    parse_list(&serde_json::from_str::<Value>(&body).ok()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // Vectors generated with node webcrypto (same primitives as the validated
    // prototype /tmp/zai-p9.cjs), secret = "testsecret", api id "testid".
    const HMAC_KEY_HEX: &str = "43a51a71ef83927a82311f75c93f3f1934edae14436e5ba8cd53c152d8ce9856";
    const AES_KEY_HEX: &str = "75a4626b4c073923316f8b2f5a15b1198788b387777f1e50da4aaca5f7474668";
    const HANDSHAKE_SIG: &str = "B9qUkm6ivKObb/F6MNxPCTAfb7vpG/mzBVJsocajvSs=";
    const PKCS8_HEX: &str =
        "302e020100300506032b6570042204200707070707070707070707070707070707070707070707070707070707070707";
    const PRIVATE_CIPHER: &str = "AAECAwQFBgcICQoLs5uiEmjdPcKqD8QXw7NUr5fv9pJGLAzxIdruHIvo3y3Rm5zaX5FiHw83+YcRl/bW7wuVRcDK2op6VIkwVcYg7zEjT3Lyw3Hj53wj1adaCBY=";
    const ED_MSG: &str =
        "testid\n1791600001000\n0.0.0-dev\ndeadbeefdeadbeefdeadbeefdeadbeef\ncafebabe00000000cafebabe00000000";
    const ED_SIG: &str = "0l8k+5BxULH2kcBNYPkKElWtJqrd/gu1n/hugEyyHHR3Phvgqfh3cb31Oc9mAhEYcTLGnG10X/VgK7X3dpxUAg==";
    const POW_SEED: &str = "3f4344cf417323dff8edc3672a7558d6";
    const POW_START: &str = "a1b2c3d4e5f6a7b8c9d0e1f2";
    const POW_ANSWER: &str = "a1b2c3d4e5f6a7b8c9d0e1f2000000f1";

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn hkdf_matches_node_vector() {
        assert_eq!(hex(hkdf32(b"testsecret", INFO_SIGN_KEY).as_slice()), HMAC_KEY_HEX);
        assert_eq!(hex(hkdf32(b"testsecret", INFO_ED25519).as_slice()), AES_KEY_HEX);
    }

    #[test]
    fn handshake_sig_matches_node_vector() {
        let sig = handshake_sig(
            b"testsecret",
            "testid",
            "1791600000000",
            "00112233445566778899aabbccddeeff",
        );
        assert_eq!(sig, HANDSHAKE_SIG);
    }

    #[test]
    fn decrypt_private_cipher_matches_node_vector() {
        let key: [u8; 32] = unhex(AES_KEY_HEX).try_into().unwrap();
        let plain = decrypt_pkcs8(&key, "testid", PRIVATE_CIPHER).expect("decrypts");
        let plain_b64 = std::str::from_utf8(plain.as_slice()).unwrap();
        assert_eq!(hex(&b64_decode(plain_b64).unwrap()), PKCS8_HEX);
        assert!(
            decrypt_pkcs8(&key, "other", PRIVATE_CIPHER).is_none(),
            "AAD is the api id"
        );
        assert!(decrypt_pkcs8(&key, "testid", "!!!not-base64").is_none());
        assert!(decrypt_pkcs8(&key, "testid", "AAAA").is_none(), "too short");
    }

    #[test]
    fn ed25519_sign_matches_node_vector() {
        let pkcs8 = unhex(PKCS8_HEX);
        let kp = Ed25519KeyPair::from_pkcs8_maybe_unchecked(&pkcs8).unwrap();
        assert_eq!(sign_b64(&kp, ED_MSG), ED_SIG);
    }

    #[test]
    fn keypair_extraction_rejects_malformed_decrypted_key() {
        let ctx = super::super::testutil::Fixture::new();
        let http = super::super::testutil::FakeHttp::new().route(
            "https://api.z.ai/api/paas/",
            200,
            r#"{"data":{"privateCipher":"AAECAwQFBgcICQoLs5uiEmjdPcKqD8QXw7NUr5fv9pJGLAzxIdruHIvo3y3Rm5zaX5FiHw83+YcRl/bW7wuVRcDK2op6VIkwVcYg7zEjT3Lyw3Hj53wj1adaCBY="}}"#,
        );
        let c = super::super::creds::Ctx {
            http: &http,
            env: &ctx,
            now: ctx.now,
            enrich_resets: true,
        };
        assert!(obtain_keypair(&c, "invalid-key-id.testsecret", "invalid-key-id", b"testsecret").is_none());
    }

    #[test]
    fn pow_seed_and_solution_match_node_vector() {
        assert_eq!(
            pow_seed_hex("testid", "deadbeefdeadbeefdeadbeefdeadbeef", "1791600001000"),
            POW_SEED
        );
        assert_eq!(solve_pow(POW_SEED, POW_START).unwrap(), POW_ANSWER);
        assert!(solve_pow("bad", POW_START).is_none());
        assert!(solve_pow(POW_SEED, "bad").is_none());
    }

    #[test]
    fn base64_round_trip_and_rejects_garbage() {
        for raw in ["", "f", "fo", "foo", "foob", "fooba", "foobar"] {
            assert_eq!(b64_decode(&b64_encode(raw.as_bytes())).unwrap(), raw.as_bytes());
        }
        assert_eq!(b64_encode(b"foobar"), "Zm9vYmFy");
        assert!(b64_decode("a").is_none());
        assert!(b64_decode("**").is_none());
        assert!(b64_decode("A===").is_none());
        assert!(b64_decode("AA=A").is_none());
        assert!(b64_decode("AA=").is_none());
    }

    const LIST: &str = r#"{"code":200,"data":{
        "lastFiveHourResetTime":"2026-10-22 11:02:31","lastWeekResetTime":"2026-10-19 23:59:59",
        "fiveHourResets":[
          {"recordId":"a","grantType":"DIRECT","expireTime":"2026-10-28 11:02:31","available":true},
          {"recordId":"b","grantType":"DIRECT","expireTime":"2026-10-27 10:00:00","available":true},
          {"recordId":"c","grantType":"DIRECT","expireTime":"2026-10-17 00:00:00","available":false},
          {"recordId":"d","grantType":"DIRECT","expireTime":"2026-10-26 09:30:00","available":true}],
        "weekResets":[
          {"recordId":"e","grantType":"DIRECT","expireTime":"2026-10-28 11:02:31","available":true},
          {"recordId":"f","grantType":"DIRECT","expireTime":"2026-10-28 11:02:31","available":true},
          {"recordId":"g","grantType":"GLOBAL","expireTime":"2026-12-22 23:59:59","available":false},
          {"recordId":"h","grantType":"GLOBAL","expireTime":"2026-12-22 23:59:59","available":false}]}}"#;

    #[test]
    fn parse_list_counts_available_and_groups_raw_expiries() {
        let (five, week) = parse_list(&serde_json::from_str::<Value>(LIST).unwrap()).unwrap();
        assert_eq!(five.available, 3);
        assert!(
            five.expires.is_empty(),
            "timezone-unknown expiry must not become an epoch"
        );
        assert_eq!(five.expiry_groups.as_ref().unwrap().len(), 3);
        assert_eq!(week.available, 2);
        assert!(
            week.expires.is_empty(),
            "timezone-unknown expiry must not become an epoch"
        );
        assert_eq!(
            five.expiry_groups.as_ref().unwrap()[0].expires_at_raw,
            "2026-10-26 09:30:00"
        );
        assert_eq!(
            week.expiry_groups.as_ref().unwrap(),
            &[ResetExpiryGroup {
                available: 2,
                expires_at_raw: "2026-10-28 11:02:31".to_owned(),
            }]
        );
    }

    #[test]
    fn parse_list_tolerates_weird_payloads() {
        // missing fields: unknown resets are still counted, missing expire skipped
        let v = json!({"data": {"fiveHourResets": [{"available": true}], "weekResets": []}});
        let (five, week) = parse_list(&v).unwrap();
        assert_eq!(
            five,
            ResetCredits {
                available: 1,
                expires: vec![],
                expiry_groups: Some(vec![]),
            }
        );
        assert_eq!(week.available, 0);
        // weird timestamps are dropped but availability survives
        let v = json!({"data": {"fiveHourResets": [{"available": true, "expireTime": "2026/10/28"}, {"available": true, "expireTime": 123}], "weekResets": []}});
        let (five, _) = parse_list(&v).unwrap();
        assert_eq!(five.available, 2);
        assert!(five.expires.is_empty());
        assert_eq!(
            five.expiry_groups.as_ref().unwrap(),
            &[ResetExpiryGroup {
                available: 1,
                expires_at_raw: "2026/10/28".to_owned(),
            }]
        );
        // malformed overall → None (enrichment must skip)
        for bad in [
            "{}",
            r#"{"code":4011,"msg":"denied"}"#,
            r#"{"data":{"fiveHourResets": 3}}"#,
        ] {
            assert!(
                parse_list(&serde_json::from_str::<Value>(bad).unwrap()).is_none(),
                "{bad}"
            );
        }
    }

    #[test]
    fn groups_all_expiry_shapes_by_exact_raw_text() {
        let input = json!({"data": {
            "fiveHourResets": [
                {"available": true, "expireTime": "raw timestamp"},
                {"available": true, "expireTime": "raw timestamp"},
                {"available": true, "expireTime": "RAW timestamp"},
                {"available": true},
                {"available": false, "expireTime": "ignored"}
            ],
            "weekResets": []
        }});
        let (five, _) = parse_list(&input).unwrap();
        assert_eq!(five.available, 4);
        assert_eq!(
            five.expiry_groups.unwrap(),
            [
                ResetExpiryGroup {
                    available: 1,
                    expires_at_raw: "RAW timestamp".to_owned()
                },
                ResetExpiryGroup {
                    available: 2,
                    expires_at_raw: "raw timestamp".to_owned()
                },
            ]
        );
    }

    #[test]
    fn keypair_cache_is_bounded_and_evicts_without_sensitive_map_keys() {
        let mut cache = HashMap::new();
        let pkcs8 = unhex(PKCS8_HEX);
        for i in 0..=KEYPAIR_CACHE_CAPACITY {
            let kp = Arc::new(Ed25519KeyPair::from_pkcs8_maybe_unchecked(&pkcs8).unwrap());
            insert_cached_keypair(&mut cache, format!("digest-{i}"), kp);
        }
        assert_eq!(cache.len(), KEYPAIR_CACHE_CAPACITY);
        assert!(cache.contains_key(&format!("digest-{KEYPAIR_CACHE_CAPACITY}")));
    }

    #[test]
    fn base64_decoder_rejects_bad_padding_and_noncanonical_tails() {
        for malformed in ["A=", "AA=A", "A===", "AB==", "AAB="] {
            assert_eq!(b64_decode(malformed), None, "{malformed}");
        }
        assert_eq!(b64_decode(""), Some(Vec::new()));
        assert_eq!(b64_decode("AA=="), Some(vec![0]));
        assert_eq!(b64_decode("AAA="), Some(vec![0, 0]));
    }

    #[test]
    fn split_key_uses_secret_after_first_dot() {
        assert_eq!(split_key("id.sec.ret"), Some(("id", "sec.ret")));
        assert_eq!(split_key("idsecret"), None);
        assert_eq!(split_key(".secret"), None);
        assert_eq!(split_key("id."), None);
    }
}
