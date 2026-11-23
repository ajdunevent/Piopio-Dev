//! OMEMO 2 (`urn:xmpp:omemo:2`, XEP-0384 0.8 and later).
//!
//! Same building blocks as legacy OMEMO, with the parameters XEP-0384 now
//! defines: Ed25519 identity keys on the wire (the same key pair as our
//! legacy identity, in its Edwards form, signing with XEdDSA, which
//! verifies as Ed25519), "OMEMO …" HKDF labels, a 16-byte MAC over the
//! X3DH associated data and the serialized OMEMOMessage, and a payload
//! sealed with AES-256-CBC + HMAC whose key and tag travel in the key
//! elements. The payload is an SCE envelope (XEP-0420), built by the caller.

use std::collections::{BTreeMap, VecDeque};

use cipher::{BlockDecryptMut, BlockEncryptMut, KeyIvInit, block_padding::Pkcs7};
use serde::{Deserialize, Serialize};
use zeroize::Zeroize;

use crate::{Error, KeyPair, PRE_KEY_COUNT, hkdf, hmac, kdf_ck, proto, random, xeddsa};

pub const NAMESPACE: &str = "urn:xmpp:omemo:2";
pub const DEVICES_NODE: &str = "urn:xmpp:omemo:2:devices";
pub const BUNDLES_NODE: &str = "urn:xmpp:omemo:2:bundles";

const MAC_LENGTH: usize = 16;
const MAX_SKIP: u32 = 1000;
const MAX_STORED_SKIPPED: usize = 1000;

/// The Ed25519 form of an identity key pair's public key.
pub fn identity_public(identity: &KeyPair) -> [u8; 32] {
    xeddsa::edwards_public(&identity.public).expect("a valid Curve25519 key")
}

/// A device's published keys (from `<bundle xmlns='urn:xmpp:omemo:2'>`).
#[derive(Clone, Debug, PartialEq)]
pub struct Bundle {
    /// Ed25519.
    pub identity_key: [u8; 32],
    pub signed_pre_key_id: u32,
    /// Curve25519.
    pub signed_pre_key: [u8; 32],
    pub signed_pre_key_signature: [u8; 64],
    pub pre_keys: Vec<(u32, [u8; 32])>,
}

/// This device's OMEMO 2 keys. The identity is shared with legacy OMEMO.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OwnKeys {
    pub device_id: u32,
    pub identity: KeyPair,
    pub signed_pre_key_id: u32,
    signed_pre_key: KeyPair,
    #[serde(with = "crate::sig")]
    signed_pre_key_signature: [u8; 64],
    old_signed_pre_key: Option<(u32, KeyPair)>,
    pre_keys: BTreeMap<u32, KeyPair>,
    next_pre_key_id: u32,
}

impl OwnKeys {
    pub fn new(device_id: u32, identity: KeyPair) -> Self {
        let signed_pre_key = KeyPair::generate();
        let signature = xeddsa::sign(&identity.private, &signed_pre_key.public);
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

    pub fn rotate_signed_pre_key(&mut self) {
        let old = std::mem::replace(&mut self.signed_pre_key, KeyPair::generate());
        self.old_signed_pre_key = Some((self.signed_pre_key_id, old));
        self.signed_pre_key_id = self.signed_pre_key_id.wrapping_add(1).max(1);
        self.signed_pre_key_signature =
            xeddsa::sign(&self.identity.private, &self.signed_pre_key.public);
    }

    pub fn bundle(&self) -> Bundle {
        Bundle {
            identity_key: identity_public(&self.identity),
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

fn kdf_rk(root: &[u8; 32], dh: &[u8; 32]) -> ([u8; 32], [u8; 32]) {
    let mut out = [0u8; 64];
    hkdf(root, dh, b"OMEMO Root Chain", &mut out);
    let (mut rk, mut ck) = ([0u8; 32], [0u8; 32]);
    rk.copy_from_slice(&out[..32]);
    ck.copy_from_slice(&out[32..]);
    out.zeroize();
    (rk, ck)
}

/// AES-256-CBC key, HMAC key and IV derived from 32 bytes with `info`.
fn split_keys(secret: &[u8], info: &[u8]) -> ([u8; 32], [u8; 32], [u8; 16]) {
    let mut out = [0u8; 80];
    hkdf(&[0u8; 32], secret, info, &mut out);
    let (mut enc, mut auth, mut iv) = ([0u8; 32], [0u8; 32], [0u8; 16]);
    enc.copy_from_slice(&out[..32]);
    auth.copy_from_slice(&out[32..64]);
    iv.copy_from_slice(&out[64..]);
    out.zeroize();
    (enc, auth, iv)
}

fn cbc_encrypt(key: &[u8; 32], iv: &[u8; 16], plaintext: &[u8]) -> Vec<u8> {
    cbc::Encryptor::<aes::Aes256>::new(key.into(), iv.into())
        .encrypt_padded_vec_mut::<Pkcs7>(plaintext)
}

fn cbc_decrypt(key: &[u8; 32], iv: &[u8; 16], ciphertext: &[u8]) -> Result<Vec<u8>, Error> {
    cbc::Decryptor::<aes::Aes256>::new(key.into(), iv.into())
        .decrypt_padded_vec_mut::<Pkcs7>(ciphertext)
        .map_err(|_| Error::Decrypt)
}

fn shared_secret(dhs: &[[u8; 32]]) -> [u8; 32] {
    let mut ikm = vec![0xffu8; 32];
    for dh in dhs {
        ikm.extend_from_slice(dh);
    }
    let mut out = [0u8; 32];
    hkdf(&[0u8; 32], &ikm, b"OMEMO X3DH", &mut out);
    ikm.zeroize();
    out
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PendingKeyExchange {
    pre_key_id: u32,
    signed_pre_key_id: u32,
    ephemeral: [u8; 32],
}

/// One OMEMO 2 Double Ratchet session with one device.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Session {
    /// Ed25519.
    pub remote_identity: [u8; 32],
    /// Ed25519.
    local_identity: [u8; 32],
    /// IK_initiator || IK_responder.
    associated_data: Vec<u8>,
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
    /// The ephemeral key of the key exchange that made this session.
    pub base_key: [u8; 32],
}

/// An encrypted ratchet message for a `<key>` element.
pub struct Encrypted {
    pub data: Vec<u8>,
    /// True if this is an OMEMOKeyExchange (`kex="true"`).
    pub kex: bool,
}

/// Starts a session with a device from its bundle.
pub fn initiate(own: &OwnKeys, bundle: &Bundle) -> Result<Session, Error> {
    if !xeddsa::verify_ed(
        &bundle.identity_key,
        &bundle.signed_pre_key,
        &bundle.signed_pre_key_signature,
    ) {
        return Err(Error::BadSignature);
    }
    if bundle.pre_keys.is_empty() {
        return Err(Error::UnknownPreKey);
    }
    let their_identity = xeddsa::montgomery_public(&bundle.identity_key).ok_or(Error::Malformed)?;
    let pick = u32::from_le_bytes(random()) as usize % bundle.pre_keys.len();
    let (pre_key_id, pre_key) = bundle.pre_keys[pick];
    let ephemeral = KeyPair::generate();
    let mut dhs = vec![
        own.identity.dh(&bundle.signed_pre_key),
        ephemeral.dh(&their_identity),
        ephemeral.dh(&bundle.signed_pre_key),
        ephemeral.dh(&pre_key),
    ];
    let sk = shared_secret(&dhs);
    dhs.zeroize();
    let local_identity = identity_public(&own.identity);
    let mut associated_data = local_identity.to_vec();
    associated_data.extend_from_slice(&bundle.identity_key);

    let sending_ratchet = KeyPair::generate();
    let (root, sending_chain) = kdf_rk(&sk, &sending_ratchet.dh(&bundle.signed_pre_key));
    Ok(Session {
        remote_identity: bundle.identity_key,
        local_identity,
        associated_data,
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
            pre_key_id,
            signed_pre_key_id: bundle.signed_pre_key_id,
            ephemeral: ephemeral.public,
        }),
        base_key: ephemeral.public,
    })
}

/// A received OMEMOKeyExchange.
pub struct KeyExchange {
    pub pre_key_id: u32,
    pub signed_pre_key_id: u32,
    /// Ed25519.
    pub identity_key: [u8; 32],
    pub base_key: [u8; 32],
    message: Vec<u8>,
}

fn key32(data: Option<&[u8]>) -> Result<[u8; 32], Error> {
    data.and_then(|d| d.try_into().ok()).ok_or(Error::Malformed)
}

pub fn parse_key_exchange(data: &[u8]) -> Result<KeyExchange, Error> {
    let f = proto::parse(data)?;
    Ok(KeyExchange {
        pre_key_id: f.uint(1).ok_or(Error::Malformed)?,
        signed_pre_key_id: f.uint(2).ok_or(Error::Malformed)?,
        identity_key: key32(f.bytes(3))?,
        base_key: key32(f.bytes(4))?,
        message: f.bytes(5).ok_or(Error::Malformed)?.to_vec(),
    })
}

impl KeyExchange {
    /// Builds the passive side of the session and decrypts the first
    /// message. The used pre key is removed from `own`.
    pub fn accept(&self, own: &mut OwnKeys) -> Result<(Session, Vec<u8>), Error> {
        let signed = own
            .signed_pre_key(self.signed_pre_key_id)
            .ok_or(Error::UnknownSignedPreKey)?
            .clone();
        let one_time = own
            .pre_keys
            .get(&self.pre_key_id)
            .ok_or(Error::UnknownPreKey)?
            .clone();
        let their_identity =
            xeddsa::montgomery_public(&self.identity_key).ok_or(Error::Malformed)?;
        let mut dhs = vec![
            signed.dh(&their_identity),
            own.identity.dh(&self.base_key),
            signed.dh(&self.base_key),
            one_time.dh(&self.base_key),
        ];
        let sk = shared_secret(&dhs);
        dhs.zeroize();
        let local_identity = identity_public(&own.identity);
        let mut associated_data = self.identity_key.to_vec();
        associated_data.extend_from_slice(&local_identity);
        let mut session = Session {
            remote_identity: self.identity_key,
            local_identity,
            associated_data,
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
        };
        let plaintext = session.decrypt(&self.message)?;
        own.pre_keys.remove(&self.pre_key_id);
        Ok((session, plaintext))
    }

    pub fn decrypt_with(&self, session: &mut Session) -> Result<Vec<u8>, Error> {
        session.decrypt(&self.message)
    }
}

impl Session {
    /// Encrypts the payload's key material (48 bytes).
    pub fn encrypt(&mut self, plaintext: &[u8]) -> Encrypted {
        let chain = match self.sending_chain {
            Some(c) => c,
            None => {
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
        let (enc, auth, iv) = split_keys(&mk, b"OMEMO Message Key Material");
        let ciphertext = cbc_encrypt(&enc, &iv, plaintext);

        let mut m = proto::Writer::default();
        m.uint(1, self.sent);
        m.uint(2, self.previous_sent);
        m.bytes(3, &self.sending_ratchet.public);
        m.bytes(4, &ciphertext);
        self.sent += 1;
        let mac = hmac(&auth, &[&self.associated_data, &m.0]);
        let mut a = proto::Writer::default();
        a.bytes(1, &mac[..MAC_LENGTH]);
        a.bytes(2, &m.0);

        match &self.pending {
            None => Encrypted {
                data: a.0,
                kex: false,
            },
            Some(p) => {
                let mut k = proto::Writer::default();
                k.uint(1, p.pre_key_id);
                k.uint(2, p.signed_pre_key_id);
                k.bytes(3, &self.local_identity);
                k.bytes(4, &p.ephemeral);
                k.bytes(5, &a.0);
                Encrypted {
                    data: k.0,
                    kex: true,
                }
            }
        }
    }

    /// Decrypts an OMEMOAuthenticatedMessage.
    pub fn decrypt(&mut self, data: &[u8]) -> Result<Vec<u8>, Error> {
        let a = proto::parse(data)?;
        let mac = a.bytes(1).ok_or(Error::Malformed)?;
        let message = a.bytes(2).ok_or(Error::Malformed)?;
        let f = proto::parse(message)?;
        let counter = f.uint(1).ok_or(Error::Malformed)?;
        let previous = f.uint(2).unwrap_or(0);
        let ratchet = key32(f.bytes(3))?;
        let ciphertext = f.bytes(4).ok_or(Error::Malformed)?;

        let mut next = self.clone();
        let mk = next.message_key(ratchet, counter, previous)?;
        let (enc, auth, iv) = split_keys(&mk, b"OMEMO Message Key Material");
        let expected = hmac(&auth, &[&self.associated_data, message]);
        if mac.len() != MAC_LENGTH || expected[..MAC_LENGTH] != *mac {
            return Err(Error::BadMac);
        }
        let plaintext = cbc_decrypt(&enc, &iv, ciphertext)?;
        next.pending = None;
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
            self.receiving_ratchet = Some(ratchet);
            let (root, chain) = kdf_rk(&self.root, &self.sending_ratchet.dh(&ratchet));
            self.root = root;
            self.receiving_chain = Some(chain);
            self.received = 0;
            self.sending_chain = None;
        }
        self.skip_until(counter)?;
        let chain = self.receiving_chain.expect("set above");
        let (mk, next) = kdf_ck(&chain);
        self.receiving_chain = Some(next);
        self.received += 1;
        Ok(mk)
    }

    fn skip_until(&mut self, until: u32) -> Result<(), Error> {
        if until < self.received || until - self.received > MAX_SKIP {
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

    pub fn is_pending(&self) -> bool {
        self.pending.is_some()
    }
}

/// A sealed payload and the 48 bytes (key and MAC) to give each device.
pub struct Payload {
    pub key_material: [u8; 48],
    pub ciphertext: Vec<u8>,
}

/// Seals a payload (an SCE envelope) with a fresh key.
pub fn encrypt_payload(plaintext: &[u8]) -> Payload {
    let key: [u8; 32] = random();
    let (enc, auth, iv) = split_keys(&key, b"OMEMO Payload");
    let ciphertext = cbc_encrypt(&enc, &iv, plaintext);
    let mac = hmac(&auth, &[&ciphertext]);
    let mut key_material = [0u8; 48];
    key_material[..32].copy_from_slice(&key);
    key_material[32..].copy_from_slice(&mac[..16]);
    Payload {
        key_material,
        ciphertext,
    }
}

pub fn decrypt_payload(key_material: &[u8], ciphertext: &[u8]) -> Result<Vec<u8>, Error> {
    if key_material.len() != 48 {
        return Err(Error::Malformed);
    }
    let (enc, auth, iv) = split_keys(&key_material[..32], b"OMEMO Payload");
    let mac = hmac(&auth, &[ciphertext]);
    if mac[..16] != key_material[32..] {
        return Err(Error::BadMac);
    }
    cbc_decrypt(&enc, &iv, ciphertext)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversation() {
        let alice = OwnKeys::new(1, KeyPair::generate());
        let mut bob = OwnKeys::new(2, KeyPair::generate());
        let mut a = initiate(&alice, &bob.bundle()).unwrap();
        let first = a.encrypt(b"hello");
        assert!(first.kex);
        let kex = parse_key_exchange(&first.data).unwrap();
        let (mut b, plain) = kex.accept(&mut bob).unwrap();
        assert_eq!(plain, b"hello");
        assert!(kex.accept(&mut bob).is_err(), "pre keys are single use");
        let reply = b.encrypt(b"hi");
        assert!(!reply.kex);
        assert_eq!(a.decrypt(&reply.data).unwrap(), b"hi");
        let m1 = a.encrypt(b"one");
        let m2 = a.encrypt(b"two");
        assert!(!m1.kex);
        assert_eq!(b.decrypt(&m2.data).unwrap(), b"two");
        assert_eq!(b.decrypt(&m1.data).unwrap(), b"one");
        assert!(b.decrypt(&m1.data).is_err(), "no replays");
    }

    #[test]
    fn payload() {
        let p = encrypt_payload(b"<envelope/>");
        assert_eq!(
            decrypt_payload(&p.key_material, &p.ciphertext).unwrap(),
            b"<envelope/>"
        );
        let mut bad = p.key_material;
        bad[40] ^= 1;
        assert!(decrypt_payload(&bad, &p.ciphertext).is_err());
    }
}
