//! The client side of SRP-6a as HomeKit pairing uses it: the 3072-bit group, SHA-512, the user
//! name "Pair-Setup" and the code shown on the screen as password.

use num_bigint::BigUint;
use ring::digest::{digest, SHA512};

/// The 3072-bit group of RFC 5054.
const PRIME_3072: &str = "\
FFFFFFFFFFFFFFFFC90FDAA22168C234C4C6628B80DC1CD129024E088A67CC74\
020BBEA63B139B22514A08798E3404DDEF9519B3CD3A431B302B0A6DF25F1437\
4FE1356D6D51C245E485B576625E7EC6F44C42E9A637ED6B0BFF5CB6F406B7ED\
EE386BFB5A899FA5AE9F24117C4B1FE649286651ECE45B3DC2007CB8A163BF05\
98DA48361C55D39A69163FA8FD24CF5F83655D23DCA3AD961C62F356208552BB\
9ED529077096966D670C354E4ABC9804F1746C08CA18217C32905E462E36CE3B\
E39E772C180E86039B2783A2EC07A28FB5C55DF06F4C52C9DE2BCBF695581718\
3995497CEA956AE515D2261898FA051015728E5A8AAAC42DAD33170D04507A33\
A85521ABDF1CBA64ECFB850458DBEF0A8AEA71575D060C7DB3970F85A6E1E4C7\
ABF5AE8CDB0933D71E8C94E04A25619DCEE3D2261AD2EE6BF12FFA06D98A0864\
D87602733EC86A64521F2B18177B200CBBE117577A615D6C770988C0BAD946E2\
08E24FA074E5AB3143DB5BFCE0FD108E4B82D120A93AD2CAFFFFFFFFFFFFFFFF";

const GENERATOR: u32 = 5;
/// Bytes of the group: public values are padded to it where the protocol says PAD().
const WIDTH: usize = 384;

fn hash(parts: &[&[u8]]) -> Vec<u8> {
    digest(&SHA512, &parts.concat()).as_ref().to_vec()
}

fn prime() -> BigUint {
    BigUint::parse_bytes(PRIME_3072.as_bytes(), 16).unwrap_or_default()
}

fn pad(value: &BigUint) -> Vec<u8> {
    let bytes = value.to_bytes_be();
    let mut padded = vec![0u8; WIDTH.saturating_sub(bytes.len())];
    padded.extend_from_slice(&bytes);
    padded
}

fn number(bytes: &[u8]) -> BigUint {
    BigUint::from_bytes_be(bytes)
}

/// What both sides derive once the password is known.
#[derive(Debug, PartialEq)]
pub struct Session {
    /// The shared key K.
    pub key: Vec<u8>,
    /// Our proof M1, sent to the device.
    pub proof: Vec<u8>,
    /// The proof M2 the device must answer with.
    pub expected: Vec<u8>,
}

pub struct Client {
    private: BigUint,
    /// Our public value A, as sent.
    pub public: Vec<u8>,
}

impl Client {
    /// Starts from random private bytes.
    pub fn new(private: &[u8]) -> Client {
        let private = number(private);
        let public = pad(&BigUint::from(GENERATOR).modpow(&private, &prime()));
        Client { private, public }
    }

    /// Derives the session from the device's salt and public value; `None` when the device sent
    /// a value the protocol forbids.
    pub fn session(
        &self,
        user: &str,
        password: &str,
        salt: &[u8],
        device_public: &[u8],
    ) -> Option<Session> {
        let modulus = prime();
        let generator = BigUint::from(GENERATOR);
        let device = number(device_public);
        if (&device % &modulus) == BigUint::default() {
            return None;
        }
        let multiplier = number(&hash(&[&modulus.to_bytes_be(), &pad(&generator)]));
        let scramble = number(&hash(&[&self.public, &pad(&device)]));
        if scramble == BigUint::default() {
            return None;
        }
        let identity = hash(&[format!("{user}:{password}").as_bytes()]);
        let secret = number(&hash(&[salt, &identity]));
        let verifier = generator.modpow(&secret, &modulus);
        // (B - k·v) mod N, kept positive.
        let base = (&device + &modulus - (&multiplier * &verifier) % &modulus) % &modulus;
        let exponent = &self.private + &scramble * &secret;
        let premaster = base.modpow(&exponent, &modulus);
        let key = hash(&[&premaster.to_bytes_be()]);

        let group: Vec<u8> = hash(&[&modulus.to_bytes_be()])
            .iter()
            .zip(hash(&[&generator.to_bytes_be()]))
            .map(|(left, right)| left ^ right)
            .collect();
        let proof = hash(&[
            &group,
            &hash(&[user.as_bytes()]),
            salt,
            &self.public,
            device_public,
            &key,
        ]);
        let expected = hash(&[&self.public, &proof, &key]);
        Some(Session {
            key,
            proof,
            expected,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The device's side of the exchange, to check that both ends agree.
    fn device(
        user: &str,
        password: &str,
        salt: &[u8],
        private: &[u8],
        client_public: &[u8],
    ) -> (Vec<u8>, Vec<u8>) {
        let modulus = prime();
        let generator = BigUint::from(GENERATOR);
        let identity = hash(&[format!("{user}:{password}").as_bytes()]);
        let secret = number(&hash(&[salt, &identity]));
        let verifier = generator.modpow(&secret, &modulus);
        let multiplier = number(&hash(&[&modulus.to_bytes_be(), &pad(&generator)]));
        let private = number(private);
        let public = (&multiplier * &verifier + generator.modpow(&private, &modulus)) % &modulus;
        let scramble = number(&hash(&[client_public, &pad(&public)]));
        let client = number(client_public);
        let premaster = (client * verifier.modpow(&scramble, &modulus)).modpow(&private, &modulus);
        (pad(&public), hash(&[&premaster.to_bytes_be()]))
    }

    #[test]
    fn both_sides_derive_the_same_key() {
        let client = Client::new(&[0x42; 32]);
        assert_eq!(client.public.len(), WIDTH);
        let salt = [9u8; 16];
        let (device_public, device_key) =
            device("Pair-Setup", "1234", &salt, &[0x17; 32], &client.public);
        let session = client
            .session("Pair-Setup", "1234", &salt, &device_public)
            .unwrap();
        assert_eq!(session.key, device_key);
        assert_eq!(
            (
                session.key.len(),
                session.proof.len(),
                session.expected.len()
            ),
            (64, 64, 64)
        );

        let wrong = client
            .session("Pair-Setup", "9999", &salt, &device_public)
            .unwrap();
        assert_ne!(wrong.key, device_key);
        assert_ne!(wrong.proof, session.proof);
    }

    #[test]
    fn refuses_forbidden_values() {
        let client = Client::new(&[1; 32]);
        assert!(client
            .session("Pair-Setup", "1234", &[0; 16], &[0; 384])
            .is_none());
        assert!(client
            .session("Pair-Setup", "1234", &[0; 16], &prime().to_bytes_be())
            .is_none());
    }

    #[test]
    fn the_group_is_the_expected_size() {
        assert_eq!(prime().bits(), 3072);
        assert_eq!(pad(&BigUint::from(5u32)).len(), WIDTH);
    }
}
