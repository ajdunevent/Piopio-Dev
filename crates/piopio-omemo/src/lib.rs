//! OMEMO (XEP-0384) for Piopio.
//!
//! This crate implements the protocol only; every primitive comes from audited
//! crates (dalek, RustCrypto). It is sans-I/O: callers keep [`OwnKeys`] and
//! [`Session`]s in their own storage (both serialize with serde) and move the
//! bytes over XMPP themselves.
//!
//! The wire format is legacy OMEMO, `eu.siacs.conversations.axolotl` (0.3.0),
//! which Conversations and Dino speak: libsignal's X3DH and Double Ratchet,
//! Curve25519 identity keys with XEdDSA signatures, AES-256-CBC with a
//! truncated HMAC per ratchet message, and AES-128-GCM for the payload.

mod proto;
pub mod xeddsa;

use std::collections::{BTreeMap, VecDeque};

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes128Gcm, Nonce};
use cipher::{BlockDecryptMut, BlockEncryptMut, KeyIvInit, block_padding::Pkcs7};
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use x25519_dalek::{PublicKey, StaticSecret};
use zeroize::{Zeroize, ZeroizeOnDrop};

pub const NAMESPACE: &str = "eu.siacs.conversations.axolotl";
pub const DEVICE_LIST_NODE: &str = "eu.siacs.conversations.axolotl.devicelist";

/// The PEP node holding a device's bundle.
pub fn bundle_node(device_id: u32) -> String {
    format!("eu.siacs.conversations.axolotl.bundles:{device_id}")
}

const VERSION: u8 = 0x33;
const MAC_LENGTH: usize = 8;
const MAX_SKIP: u32 = 1000;
const MAX_STORED_SKIPPED: usize = 2000;
/// How many one-time pre keys the bundle offers.
pub const PRE_KEY_COUNT: usize = 100;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Error {
    #[error("malformed message")]
    Malformed,
    #[error("unsupported message version")]
    Version,
    #[error("the bundle's signature is invalid")]
    BadSignature,
    #[error("the message failed authentication")]
    BadMac,
    #[error("unknown signed pre key")]
    UnknownSignedPreKey,
    #[error("unknown or already used pre key")]
    UnknownPreKey,
    #[error("message key already used or too far in the future")]
    MessageKey,
    #[error("decryption failed")]
    Decrypt,
}

pub(crate) fn random<const N: usize>() -> [u8; N] {
    let mut out = [0u8; N];
    getrandom::fill(&mut out).expect("operating system randomness");
    out
}

/// A random device id: 1 ..= 2^31 - 1.
pub fn new_device_id() -> u32 {
    loop {
        let id = u32::from_le_bytes(random()) & 0x7fff_ffff;
        if id != 0 {
            return id;
        }
    }
}

/// A Curve25519 key pair. The private key is zeroed on drop.
#[derive(Clone, Serialize, Deserialize, Zeroize, ZeroizeOnDrop)]
pub struct KeyPair {
    private: [u8; 32],
    #[zeroize(skip)]
    pub public: [u8; 32],
}

impl KeyPair {
    pub fn generate() -> Self {
        Self::from_private(random())
    }

    fn from_private(private: [u8; 32]) -> Self {
        let private = xeddsa::clamp(private);
        let public = PublicKey::from(&StaticSecret::from(private)).to_bytes();
        Self { private, public }
    }

    fn dh(&self, their_public: &[u8; 32]) -> [u8; 32] {
        StaticSecret::from(self.private)
            .diffie_hellman(&PublicKey::from(*their_public))
            .to_bytes()
    }
}

impl std::fmt::Debug for KeyPair {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "KeyPair({})", fingerprint(&self.public))
    }
}

/// A public key in the wire encoding: 0x05 followed by 32 bytes.
pub fn encode_key(key: &[u8; 32]) -> [u8; 33] {
    let mut out = [5u8; 33];
    out[1..].copy_from_slice(key);
    out
}

/// Reads a wire-encoded public key (33 bytes with 0x05, or a bare 32).
pub fn decode_key(data: &[u8]) -> Result<[u8; 32], Error> {
    let raw = match data.len() {
        33 if data[0] == 5 => &data[1..],
        32 => data,
        _ => return Err(Error::Malformed),
    };
    let mut key = [0u8; 32];
    key.copy_from_slice(raw);
    Ok(key)
}

/// The fingerprint users compare: the identity key in hex, in groups of 8,
/// as Conversations and Dino show it.
pub fn fingerprint(identity: &[u8; 32]) -> String {
    let hex: String = identity.iter().map(|b| format!("{b:02x}")).collect();
    hex.as_bytes()
        .chunks(8)
        .map(|c| std::str::from_utf8(c).unwrap_or(""))
        .collect::<Vec<_>>()
        .join(" ")
}

/// A device's published keys (its PEP bundle).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bundle {
    pub identity_key: [u8; 32],
    pub signed_pre_key_id: u32,
    pub signed_pre_key: [u8; 32],
    #[serde(with = "sig")]
    pub signed_pre_key_signature: [u8; 64],
    pub pre_keys: Vec<(u32, [u8; 32])>,
}

mod sig {
    use serde::{Deserialize, Deserializer, Serializer};
    pub fn serialize<S: Serializer>(v: &[u8; 64], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_bytes(v)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; 64], D::Error> {
        let v: Vec<u8> = Vec::deserialize(d)?;
        v.try_into()
            .map_err(|_| serde::de::Error::custom("a signature has 64 bytes"))
    }
}

/// This device's private keys.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OwnKeys {
    pub device_id: u32,
    pub identity: KeyPair,
    pub signed_pre_key_id: u32,
    signed_pre_key: KeyPair,
    #[serde(with = "sig")]
    signed_pre_key_signature: [u8; 64],
    /// The previous signed pre key, kept for a while after rotation so that
    /// sessions started from the old bundle still work.
    old_signed_pre_key: Option<(u32, KeyPair)>,
    pre_keys: BTreeMap<u32, KeyPair>,
    next_pre_key_id: u32,
}

impl OwnKeys {
    pub fn generate(device_id: u32) -> Self {
        let identity = KeyPair::generate();
        let signed_pre_key = KeyPair::generate();
        let signature = xeddsa::sign(&identity.private, &encode_key(&signed_pre_key.public));
        let mut keys = Self {
            device_id,
            identity,
            signed_pre_key_id: 1,
            signed_pre_key,
            signed_pre_key_signature: signature,
            old_signed_pre_key: None,
            pre_keys: BTreeMap::new(),
            next_pre_key_id: 1,
        };
        keys.refill_pre_keys();
        keys
    }

    /// Tops the one-time pre keys back up to [`PRE_KEY_COUNT`]. Returns
    /// whether any were added (the bundle then needs publishing).
    pub fn refill_pre_keys(&mut self) -> bool {
        let mut added = false;
        while self.pre_keys.len() < PRE_KEY_COUNT {
            self.pre_keys
                .insert(self.next_pre_key_id, KeyPair::generate());
            self.next_pre_key_id = self.next_pre_key_id.wrapping_add(1).max(1);
            added = true;
        }
        added
    }

    /// Replaces the signed pre key, keeping the old one for late sessions.
    pub fn rotate_signed_pre_key(&mut self) {
        let old = std::mem::replace(&mut self.signed_pre_key, KeyPair::generate());
        self.old_signed_pre_key = Some((self.signed_pre_key_id, old));
        self.signed_pre_key_id = self.signed_pre_key_id.wrapping_add(1).max(1);
        self.signed_pre_key_signature = xeddsa::sign(
            &self.identity.private,
            &encode_key(&self.signed_pre_key.public),
        );
    }

    pub fn bundle(&self) -> Bundle {
        Bundle {
            identity_key: self.identity.public,
            signed_pre_key_id: self.signed_pre_key_id,
            signed_pre_key: self.signed_pre_key.public,
            signed_pre_key_signature: self.signed_pre_key_signature,
            pre_keys: self
                .pre_keys
                .iter()
                .map(|(id, k)| (*id, k.public))
                .collect(),
        }
    }

    fn signed_pre_key(&self, id: u32) -> Option<&KeyPair> {
        if id == self.signed_pre_key_id {
            return Some(&self.signed_pre_key);
        }
        self.old_signed_pre_key
            .as_ref()
            .filter(|(old, _)| *old == id)
            .map(|(_, k)| k)
    }
}

fn hkdf(salt: &[u8], ikm: &[u8], info: &[u8], out: &mut [u8]) {
    Hkdf::<Sha256>::new(Some(salt), ikm)
        .expand(info, out)
        .expect("valid HKDF length");
}

fn hmac(key: &[u8], data: &[&[u8]]) -> [u8; 32] {
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(key).expect("any key length");
    for d in data {
        mac.update(d);
    }
    mac.finalize().into_bytes().into()
}

/// The root key step: (root, dh) → (new root, chain key).
fn kdf_rk(root: &[u8; 32], dh: &[u8; 32]) -> ([u8; 32], [u8; 32]) {
    let mut out = [0u8; 64];
    hkdf(root, dh, b"WhisperRatchet", &mut out);
    let (mut rk, mut ck) = ([0u8; 32], [0u8; 32]);
    rk.copy_from_slice(&out[..32]);
    ck.copy_from_slice(&out[32..]);
    out.zeroize();
    (rk, ck)
}

/// The chain step: chain key → (message key, next chain key).
fn kdf_ck(ck: &[u8; 32]) -> ([u8; 32], [u8; 32]) {
    (hmac(ck, &[&[1]]), hmac(ck, &[&[2]]))
}

struct MessageKeys {
    cipher: [u8; 32],
    mac: [u8; 32],
    iv: [u8; 16],
}

impl MessageKeys {
    fn from(mk: &[u8; 32]) -> Self {
        let mut out = [0u8; 80];
        hkdf(&[0u8; 32], mk, b"WhisperMessageKeys", &mut out);
        let mut keys = Self {
            cipher: [0; 32],
            mac: [0; 32],
            iv: [0; 16],
        };
        keys.cipher.copy_from_slice(&out[..32]);
        keys.mac.copy_from_slice(&out[32..64]);
        keys.iv.copy_from_slice(&out[64..]);
        out.zeroize();
        keys
    }
}

impl Drop for MessageKeys {
    fn drop(&mut self) {
        self.cipher.zeroize();
        self.mac.zeroize();
    }
}

/// What a session still needs to say until the other side answers: the key
/// exchange that lets them build their end of it.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct PendingKeyExchange {
    pre_key_id: Option<u32>,
    signed_pre_key_id: u32,
    base_key: [u8; 32],
}

/// One Double Ratchet session with one device of a contact.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Session {
    pub remote_identity: [u8; 32],
    local_identity: [u8; 32],
    root: [u8; 32],
    sending_ratchet: KeyPair,
    receiving_ratchet: Option<[u8; 32]>,
    sending_chain: Option<[u8; 32]>,
    receiving_chain: Option<[u8; 32]>,
    sent: u32,
    received: u32,
    previous_sent: u32,
    skipped: VecDeque<([u8; 32], u32, [u8; 32])>,
    pending: Option<PendingKeyExchange>,
    /// The base key of the key exchange that created this session (theirs
    /// for a passive session), to recognise repeated key exchanges.
    pub base_key: [u8; 32],
    pub registration_id: u32,
}

/// An encrypted ratchet message, ready for a `<key>` element.
pub struct Encrypted {
    pub data: Vec<u8>,
    /// True if this carries a key exchange (`prekey="true"`).
    pub prekey: bool,
}

fn shared_secret(dhs: &[[u8; 32]]) -> [u8; 32] {
    let mut ikm = vec![0xffu8; 32];
    for dh in dhs {
        ikm.extend_from_slice(dh);
    }
    let mut out = [0u8; 32];
    hkdf(&[0u8; 32], &ikm, b"WhisperText", &mut out);
    ikm.zeroize();
    out
}

/// Starts a session with a device from its bundle (the active side of X3DH).
pub fn initiate(own: &OwnKeys, bundle: &Bundle) -> Result<Session, Error> {
    if !xeddsa::verify(
        &bundle.identity_key,
        &encode_key(&bundle.signed_pre_key),
        &bundle.signed_pre_key_signature,
    ) {
        return Err(Error::BadSignature);
    }
    let pre_key = if bundle.pre_keys.is_empty() {
        None
    } else {
        let pick = u32::from_le_bytes(random()) as usize % bundle.pre_keys.len();
        Some(bundle.pre_keys[pick])
    };
    let ephemeral = KeyPair::generate();
    let mut dhs = vec![
        own.identity.dh(&bundle.signed_pre_key),
        ephemeral.dh(&bundle.identity_key),
        ephemeral.dh(&bundle.signed_pre_key),
    ];
    if let Some((_, pk)) = pre_key {
        dhs.push(ephemeral.dh(&pk));
    }
    let sk = shared_secret(&dhs);
    dhs.zeroize();

    let sending_ratchet = KeyPair::generate();
    let (root, sending_chain) = kdf_rk(&sk, &sending_ratchet.dh(&bundle.signed_pre_key));
    Ok(Session {
        remote_identity: bundle.identity_key,
        local_identity: own.identity.public,
        root,
        sending_ratchet,
        receiving_ratchet: Some(bundle.signed_pre_key),
        sending_chain: Some(sending_chain),
        receiving_chain: None,
        sent: 0,
        received: 0,
        previous_sent: 0,
        skipped: VecDeque::new(),
        pending: Some(PendingKeyExchange {
            pre_key_id: pre_key.map(|(id, _)| id),
            signed_pre_key_id: bundle.signed_pre_key_id,
            base_key: ephemeral.public,
        }),
        base_key: ephemeral.public,
        registration_id: 0,
    })
}

/// A key exchange as received, before acting on it.
pub struct KeyExchange {
    pub pre_key_id: Option<u32>,
    pub signed_pre_key_id: u32,
    pub base_key: [u8; 32],
    pub identity_key: [u8; 32],
    pub registration_id: u32,
    message: Vec<u8>,
}

/// Reads a `prekey="true"` key element.
pub fn parse_key_exchange(data: &[u8]) -> Result<KeyExchange, Error> {
    match data.first() {
        Some(&VERSION) => {}
        Some(_) => return Err(Error::Version),
        None => return Err(Error::Malformed),
    }
    let f = proto::parse(&data[1..])?;
    Ok(KeyExchange {
        pre_key_id: f.uint(1),
        signed_pre_key_id: f.uint(6).ok_or(Error::Malformed)?,
        base_key: decode_key(f.bytes(2).ok_or(Error::Malformed)?)?,
        identity_key: decode_key(f.bytes(3).ok_or(Error::Malformed)?)?,
        registration_id: f.uint(5).unwrap_or(0),
        message: f.bytes(4).ok_or(Error::Malformed)?.to_vec(),
    })
}

impl KeyExchange {
    /// Builds the passive side of the session and decrypts the first message.
    /// The used one-time pre key is removed from `own`; republish the bundle.
    pub fn accept(&self, own: &mut OwnKeys) -> Result<(Session, Vec<u8>), Error> {
        let signed = own
            .signed_pre_key(self.signed_pre_key_id)
            .ok_or(Error::UnknownSignedPreKey)?
            .clone();
        let one_time = match self.pre_key_id {
            Some(id) => Some(own.pre_keys.get(&id).ok_or(Error::UnknownPreKey)?.clone()),
            None => None,
        };
        let mut dhs = vec![
            signed.dh(&self.identity_key),
            own.identity.dh(&self.base_key),
            signed.dh(&self.base_key),
        ];
        if let Some(k) = &one_time {
            dhs.push(k.dh(&self.base_key));
        }
        let sk = shared_secret(&dhs);
        dhs.zeroize();
        let mut session = Session {
            remote_identity: self.identity_key,
            local_identity: own.identity.public,
            root: sk,
            sending_ratchet: signed,
            receiving_ratchet: None,
            sending_chain: None,
            receiving_chain: None,
            sent: 0,
            received: 0,
            previous_sent: 0,
            skipped: VecDeque::new(),
            pending: None,
            base_key: self.base_key,
            registration_id: self.registration_id,
        };
        let plaintext = session.decrypt(&self.message)?;
        if let Some(id) = self.pre_key_id {
            own.pre_keys.remove(&id);
        }
        Ok((session, plaintext))
    }

    /// Decrypts a repeated key exchange with the session it already created.
    pub fn decrypt_with(&self, session: &mut Session) -> Result<Vec<u8>, Error> {
        session.decrypt(&self.message)
    }
}

impl Session {
    /// Encrypts `plaintext` (for OMEMO: the 32-byte payload key and tag).
    pub fn encrypt(&mut self, plaintext: &[u8]) -> Encrypted {
        let chain = match self.sending_chain {
            Some(c) => c,
            None => {
                // Passive side sending first after a receive: ratchet forward.
                let their = self.receiving_ratchet.expect("a session that has received");
                self.previous_sent = self.sent;
                self.sent = 0;
                self.sending_ratchet = KeyPair::generate();
                let (root, chain) = kdf_rk(&self.root, &self.sending_ratchet.dh(&their));
                self.root = root;
                chain
            }
        };
        let (mk, next) = kdf_ck(&chain);
        self.sending_chain = Some(next);
        let keys = MessageKeys::from(&mk);
        let ciphertext = cbc::Encryptor::<aes::Aes256>::new(&keys.cipher.into(), &keys.iv.into())
            .encrypt_padded_vec_mut::<Pkcs7>(plaintext);

        let mut w = proto::Writer::default();
        w.bytes(1, &encode_key(&self.sending_ratchet.public));
        w.uint(2, self.sent);
        w.uint(3, self.previous_sent);
        w.bytes(4, &ciphertext);
        self.sent += 1;
        let mut message = vec![VERSION];
        message.extend_from_slice(&w.0);
        let mac = hmac(
            &keys.mac,
            &[
                &encode_key(&self.local_identity),
                &encode_key(&self.remote_identity),
                &message,
            ],
        );
        message.extend_from_slice(&mac[..MAC_LENGTH]);

        match &self.pending {
            None => Encrypted {
                data: message,
                prekey: false,
            },
            Some(p) => {
                let mut w = proto::Writer::default();
                if let Some(id) = p.pre_key_id {
                    w.uint(1, id);
                }
                w.bytes(2, &encode_key(&p.base_key));
                w.bytes(3, &encode_key(&self.local_identity));
                w.bytes(4, &message);
                w.uint(5, self.registration_id);
                w.uint(6, p.signed_pre_key_id);
                let mut data = vec![VERSION];
                data.extend_from_slice(&w.0);
                Encrypted { data, prekey: true }
            }
        }
    }

    /// Decrypts a ratchet message (a `<key>` element without `prekey`).
    pub fn decrypt(&mut self, data: &[u8]) -> Result<Vec<u8>, Error> {
        if data.len() < 1 + MAC_LENGTH {
            return Err(Error::Malformed);
        }
        if data[0] != VERSION {
            return Err(Error::Version);
        }
        let (message, mac) = data.split_at(data.len() - MAC_LENGTH);
        let f = proto::parse(&message[1..])?;
        let ratchet = decode_key(f.bytes(1).ok_or(Error::Malformed)?)?;
        let counter = f.uint(2).ok_or(Error::Malformed)?;
        let previous = f.uint(3).unwrap_or(0);
        let ciphertext = f.bytes(4).ok_or(Error::Malformed)?;

        // Work on a copy so a forged message cannot damage the session.
        let mut next = self.clone();
        let mk = next.message_key(ratchet, counter, previous)?;
        let keys = MessageKeys::from(&mk);
        let expected = hmac(
            &keys.mac,
            &[
                &encode_key(&self.remote_identity),
                &encode_key(&self.local_identity),
                message,
            ],
        );
        if expected[..MAC_LENGTH] != *mac {
            return Err(Error::BadMac);
        }
        let plaintext = cbc::Decryptor::<aes::Aes256>::new(&keys.cipher.into(), &keys.iv.into())
            .decrypt_padded_vec_mut::<Pkcs7>(ciphertext)
            .map_err(|_| Error::Decrypt)?;
        next.pending = None; // they answered: no more key exchanges needed
        *self = next;
        Ok(plaintext)
    }

    fn message_key(
        &mut self,
        ratchet: [u8; 32],
        counter: u32,
        previous: u32,
    ) -> Result<[u8; 32], Error> {
        if let Some(i) = self
            .skipped
            .iter()
            .position(|(r, n, _)| *r == ratchet && *n == counter)
        {
            let (_, _, mk) = self.skipped.remove(i).expect("found");
            return Ok(mk);
        }
        if self.receiving_ratchet != Some(ratchet) || self.receiving_chain.is_none() {
            if self.receiving_chain.is_some() {
                self.skip_until(previous)?;
            }
            // DH ratchet step.
            self.receiving_ratchet = Some(ratchet);
            let (root, chain) = kdf_rk(&self.root, &self.sending_ratchet.dh(&ratchet));
            self.root = root;
            self.receiving_chain = Some(chain);
            self.received = 0;
            self.sending_chain = None; // the next send ratchets forward
        }
        self.skip_until(counter)?;
        let chain = self.receiving_chain.expect("set above");
        let (mk, next) = kdf_ck(&chain);
        self.receiving_chain = Some(next);
        self.received += 1;
        Ok(mk)
    }

    fn skip_until(&mut self, until: u32) -> Result<(), Error> {
        if until < self.received {
            return Err(Error::MessageKey);
        }
        if until - self.received > MAX_SKIP {
            return Err(Error::MessageKey);
        }
        let (Some(ratchet), Some(mut chain)) = (self.receiving_ratchet, self.receiving_chain)
        else {
            return Ok(());
        };
        while self.received < until {
            let (mk, next) = kdf_ck(&chain);
            self.skipped.push_back((ratchet, self.received, mk));
            if self.skipped.len() > MAX_STORED_SKIPPED {
                self.skipped.pop_front();
            }
            chain = next;
            self.received += 1;
        }
        self.receiving_chain = Some(chain);
        Ok(())
    }

    /// Whether messages still carry a key exchange (no answer received yet).
    pub fn is_pending(&self) -> bool {
        self.pending.is_some()
    }
}

/// An encrypted message payload with the key material for the `<key>` elements.
pub struct Payload {
    /// AES key (16 bytes) followed by the GCM tag (16 bytes).
    pub key_material: [u8; 32],
    pub iv: [u8; 12],
    pub ciphertext: Vec<u8>,
}

/// Encrypts a message body with a fresh AES-128-GCM key.
pub fn encrypt_payload(plaintext: &[u8]) -> Payload {
    let key: [u8; 16] = random();
    let iv: [u8; 12] = random();
    let mut sealed = Aes128Gcm::new(&key.into())
        .encrypt(Nonce::from_slice(&iv), plaintext)
        .expect("AES-GCM encryption cannot fail");
    let tag = sealed.split_off(sealed.len() - 16);
    let mut key_material = [0u8; 32];
    key_material[..16].copy_from_slice(&key);
    key_material[16..].copy_from_slice(&tag);
    Payload {
        key_material,
        iv,
        ciphertext: sealed,
    }
}

/// Decrypts a payload. Accepts the 32-byte key-and-tag form and the older
/// 16-byte key with the tag at the end of the payload. IVs of 12 or 16 bytes.
pub fn decrypt_payload(
    key_material: &[u8],
    iv: &[u8],
    ciphertext: &[u8],
) -> Result<Vec<u8>, Error> {
    let (key, sealed) = match key_material.len() {
        32 => {
            let mut sealed = ciphertext.to_vec();
            sealed.extend_from_slice(&key_material[16..]);
            (&key_material[..16], sealed)
        }
        16 => (key_material, ciphertext.to_vec()),
        _ => return Err(Error::Malformed),
    };
    let key: [u8; 16] = key.try_into().map_err(|_| Error::Malformed)?;
    match iv.len() {
        12 => Aes128Gcm::new(&key.into())
            .decrypt(Nonce::from_slice(iv), sealed.as_slice())
            .map_err(|_| Error::Decrypt),
        16 => {
            use aes_gcm::aead::consts::U16;
            aes_gcm::AesGcm::<aes::Aes128, U16>::new(&key.into())
                .decrypt(aes_gcm::Nonce::<U16>::from_slice(iv), sealed.as_slice())
                .map_err(|_| Error::Decrypt)
        }
        _ => Err(Error::Malformed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exchange(alice: &OwnKeys, bob: &mut OwnKeys) -> (Session, Session) {
        let mut a = initiate(alice, &bob.bundle()).unwrap();
        let first = a.encrypt(b"first");
        assert!(first.prekey);
        let kex = parse_key_exchange(&first.data).unwrap();
        let (b, plain) = kex.accept(bob).unwrap();
        assert_eq!(plain, b"first");
        (a, b)
    }

    #[test]
    fn conversation_with_out_of_order_and_replay() {
        let alice = OwnKeys::generate(1);
        let mut bob = OwnKeys::generate(2);
        let before = bob.bundle().pre_keys.len();
        let (mut a, mut b) = exchange(&alice, &mut bob);
        assert_eq!(bob.bundle().pre_keys.len(), before - 1);

        // Alice keeps sending key exchanges until Bob answers.
        let second = a.encrypt(b"second");
        assert!(second.prekey);
        let kex = parse_key_exchange(&second.data).unwrap();
        assert_eq!(kex.decrypt_with(&mut b).unwrap(), b"second");

        let reply = b.encrypt(b"reply");
        assert!(!reply.prekey);
        assert_eq!(a.decrypt(&reply.data).unwrap(), b"reply");
        assert!(!a.encrypt(b"x").prekey);

        // Out of order within and across ratchet steps.
        let m1 = b.encrypt(b"m1");
        let m2 = b.encrypt(b"m2");
        assert_eq!(a.decrypt(&m2.data).unwrap(), b"m2");
        let n1 = a.encrypt(b"n1");
        assert_eq!(b.decrypt(&n1.data).unwrap(), b"n1"); // skips the undelivered "x"
        let m3 = b.encrypt(b"m3");
        assert_eq!(a.decrypt(&m3.data).unwrap(), b"m3");
        assert_eq!(a.decrypt(&m1.data).unwrap(), b"m1");
        // Replays fail.
        assert!(a.decrypt(&m1.data).is_err());
        // Tampering fails and leaves the session working.
        let mut bad = b.encrypt(b"t").data;
        let len = bad.len();
        bad[len - 12] ^= 1;
        assert_eq!(a.decrypt(&bad), Err(Error::BadMac));
        let ok = b.encrypt(b"after");
        assert_eq!(a.decrypt(&ok.data).unwrap(), b"after");
    }

    #[test]
    fn pre_key_is_single_use_and_signature_checked() {
        let alice = OwnKeys::generate(1);
        let mut bob = OwnKeys::generate(2);
        let mut a = initiate(&alice, &bob.bundle()).unwrap();
        let first = a.encrypt(b"hi");
        let kex = parse_key_exchange(&first.data).unwrap();
        kex.accept(&mut bob).unwrap();
        assert_eq!(kex.accept(&mut bob).err(), Some(Error::UnknownPreKey));

        let mut forged = bob.bundle();
        forged.signed_pre_key = KeyPair::generate().public;
        assert_eq!(initiate(&alice, &forged).err(), Some(Error::BadSignature));
    }

    #[test]
    fn payload_round_trip_and_tag_check() {
        let p = encrypt_payload(b"hello payload");
        assert_eq!(
            decrypt_payload(&p.key_material, &p.iv, &p.ciphertext).unwrap(),
            b"hello payload"
        );
        let mut bad = p.key_material;
        bad[20] ^= 1;
        assert!(decrypt_payload(&bad, &p.iv, &p.ciphertext).is_err());
        // Old style: tag appended to the payload.
        let mut sealed = p.ciphertext.clone();
        sealed.extend_from_slice(&p.key_material[16..]);
        assert_eq!(
            decrypt_payload(&p.key_material[..16], &p.iv, &sealed).unwrap(),
            b"hello payload"
        );
    }

    #[test]
    fn state_survives_serde() {
        let alice = OwnKeys::generate(1);
        let mut bob = OwnKeys::generate(2);
        let (a, _) = exchange(&alice, &mut bob);
        let json = serde_json::to_string(&a).unwrap();
        let mut a2: Session = serde_json::from_str(&json).unwrap();
        let bob_json = serde_json::to_string(&bob).unwrap();
        let _bob2: OwnKeys = serde_json::from_str(&bob_json).unwrap();
        assert!(a2.encrypt(b"still works").prekey);
    }
}
