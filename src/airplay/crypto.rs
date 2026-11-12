//! The cryptography of HomeKit pairing, on top of `ring`: key derivation, the AEAD that
//! protects messages, long-term signatures and the ephemeral key exchange.

use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, CHACHA20_POLY1305};
use ring::rand::{SecureRandom, SystemRandom};
use ring::signature::{Ed25519KeyPair, KeyPair, UnparsedPublicKey, ED25519};
use ring::{agreement, hkdf};

use crate::error::Result;
use crate::fail;

pub const KEY: usize = 32;
pub const TAG: usize = 16;

struct Length(usize);

impl hkdf::KeyType for Length {
    fn len(&self) -> usize {
        self.0
    }
}

/// HKDF-SHA512 with the salt and info strings the protocol names.
pub fn derive(secret: &[u8], salt: &str, info: &str) -> Result<[u8; KEY]> {
    let mut key = [0u8; KEY];
    hkdf::Salt::new(hkdf::HKDF_SHA512, salt.as_bytes())
        .extract(secret)
        .expand(&[info.as_bytes()], Length(KEY))
        .and_then(|material| material.fill(&mut key))
        .map_err(|_| fail!("cannot derive a key"))?;
    Ok(key)
}

fn cipher(key: &[u8]) -> Result<LessSafeKey> {
    UnboundKey::new(&CHACHA20_POLY1305, key)
        .map(LessSafeKey::new)
        .map_err(|_| fail!("invalid encryption key"))
}

/// A 12-byte nonce from the 8 bytes the protocol gives (a label or a counter).
pub fn nonce(tail: [u8; 8]) -> [u8; 12] {
    let mut nonce = [0u8; 12];
    nonce[4..].copy_from_slice(&tail);
    nonce
}

/// Encrypts and appends the authentication tag.
pub fn seal(key: &[u8], nonce: [u8; 12], aad: &[u8], plain: &[u8]) -> Result<Vec<u8>> {
    let mut data = plain.to_vec();
    cipher(key)?
        .seal_in_place_append_tag(
            Nonce::assume_unique_for_key(nonce),
            Aad::from(aad),
            &mut data,
        )
        .map_err(|_| fail!("cannot encrypt"))?;
    Ok(data)
}

/// Decrypts data ending with its tag; fails when it was not made with this key.
pub fn open(key: &[u8], nonce: [u8; 12], aad: &[u8], sealed: &[u8]) -> Result<Vec<u8>> {
    let mut data = sealed.to_vec();
    let plain = cipher(key)?
        .open_in_place(
            Nonce::assume_unique_for_key(nonce),
            Aad::from(aad),
            &mut data,
        )
        .map_err(|_| fail!("cannot decrypt (wrong key or damaged data)"))?
        .len();
    data.truncate(plain);
    Ok(data)
}

pub fn random<const N: usize>() -> Result<[u8; N]> {
    let mut bytes = [0u8; N];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| fail!("no randomness available"))?;
    Ok(bytes)
}

/// A random UUID, upper case as Apple writes them.
pub fn uuid() -> Result<String> {
    let mut bytes: [u8; 16] = random()?;
    bytes[6] = (bytes[6] & 0x0F) | 0x40;
    bytes[8] = (bytes[8] & 0x3F) | 0x80;
    let hex = hex(&bytes).to_uppercase();
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    ))
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub fn unhex(text: &str) -> Option<Vec<u8>> {
    if text.len() % 2 != 0 {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(text.get(at..at + 2)?, 16).ok())
        .collect()
}

/// A long-term Ed25519 identity, kept as its 32-byte seed.
pub struct Identity {
    pair: Ed25519KeyPair,
}

impl Identity {
    pub fn from_seed(seed: &[u8]) -> Result<Identity> {
        Ed25519KeyPair::from_seed_unchecked(seed)
            .map(|pair| Identity { pair })
            .map_err(|_| fail!("invalid identity key"))
    }

    pub fn public(&self) -> Vec<u8> {
        self.pair.public_key().as_ref().to_vec()
    }

    pub fn sign(&self, message: &[u8]) -> Vec<u8> {
        self.pair.sign(message).as_ref().to_vec()
    }
}

/// Checks an Ed25519 signature.
pub fn verify(public: &[u8], message: &[u8], signature: &[u8]) -> bool {
    UnparsedPublicKey::new(&ED25519, public)
        .verify(message, signature)
        .is_ok()
}

/// One X25519 exchange: our public value, then the secret shared with the peer.
pub struct Exchange {
    private: agreement::EphemeralPrivateKey,
    pub public: Vec<u8>,
}

impl Exchange {
    pub fn new() -> Result<Exchange> {
        let private =
            agreement::EphemeralPrivateKey::generate(&agreement::X25519, &SystemRandom::new())
                .map_err(|_| fail!("no randomness available"))?;
        let public = private
            .compute_public_key()
            .map_err(|_| fail!("cannot compute a public key"))?
            .as_ref()
            .to_vec();
        Ok(Exchange { private, public })
    }

    pub fn shared(self, peer: &[u8]) -> Result<Vec<u8>> {
        agreement::agree_ephemeral(
            self.private,
            &agreement::UnparsedPublicKey::new(&agreement::X25519, peer),
            |secret| secret.to_vec(),
        )
        .map_err(|_| fail!("the device sent an invalid key"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_distinct_keys() {
        let one = derive(b"secret", "Control-Salt", "Control-Write-Encryption-Key").unwrap();
        let two = derive(b"secret", "Control-Salt", "Control-Read-Encryption-Key").unwrap();
        assert_ne!(one, two);
        assert_eq!(
            one,
            derive(b"secret", "Control-Salt", "Control-Write-Encryption-Key").unwrap()
        );
    }

    #[test]
    fn seals_and_opens() {
        let key = [3u8; KEY];
        let sealed = seal(&key, nonce(*b"PS-Msg05"), b"aad", b"hello").unwrap();
        assert_eq!(sealed.len(), 5 + TAG);
        assert_eq!(
            open(&key, nonce(*b"PS-Msg05"), b"aad", &sealed).unwrap(),
            b"hello"
        );
        assert!(open(&key, nonce(*b"PS-Msg06"), b"aad", &sealed).is_err());
        assert!(open(&key, nonce(*b"PS-Msg05"), b"other", &sealed).is_err());
        assert!(open(&[4u8; KEY], nonce(*b"PS-Msg05"), b"aad", &sealed).is_err());
        assert!(open(&key, nonce(*b"PS-Msg05"), b"aad", b"short").is_err());
        assert!(seal(&[1u8; 5], nonce([0; 8]), b"", b"x").is_err());
        assert_eq!(
            nonce(7u64.to_le_bytes()),
            [0, 0, 0, 0, 7, 0, 0, 0, 0, 0, 0, 0]
        );
    }

    #[test]
    fn signs_and_verifies() {
        let identity = Identity::from_seed(&[5u8; 32]).unwrap();
        let signature = identity.sign(b"message");
        assert!(verify(&identity.public(), b"message", &signature));
        assert!(!verify(&identity.public(), b"other", &signature));
        assert!(!verify(&[0u8; 32], b"message", &signature));
        assert!(Identity::from_seed(&[1u8; 3]).is_err());
    }

    #[test]
    fn exchanges_agree() {
        let (ours, theirs) = (Exchange::new().unwrap(), Exchange::new().unwrap());
        let (our_public, their_public) = (ours.public.clone(), theirs.public.clone());
        assert_eq!(
            ours.shared(&their_public).unwrap(),
            theirs.shared(&our_public).unwrap()
        );
        assert!(Exchange::new().unwrap().shared(&[1, 2, 3]).is_err());
    }

    #[test]
    fn formats() {
        assert_eq!(hex(&[0, 255, 16]), "00ff10");
        assert_eq!(unhex("00ff10"), Some(vec![0, 255, 16]));
        assert_eq!(unhex("0"), None);
        assert_eq!(unhex("zz"), None);
        let uuid = uuid().unwrap();
        assert_eq!(
            (uuid.len(), uuid.as_bytes()[14], uuid.matches('-').count()),
            (36, b'4', 4)
        );
        assert_ne!(random::<16>().unwrap(), random::<16>().unwrap());
    }
}
