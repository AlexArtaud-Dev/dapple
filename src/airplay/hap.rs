//! HomeKit pairing as AirPlay uses it: pair once with the code shown on the screen, then prove
//! that pairing at each connection and derive the keys that encrypt it.

use std::time::Duration;

use super::channel::{compose, Connection, Message};
use super::crypto::{self, Exchange, Identity};
use super::srp;
use super::tlv8;
use crate::error::Result;
use crate::fail;

const TIMEOUT: Duration = Duration::from_secs(15);

/// What a pairing leaves on each side: who we are for the device, and who the device is.
#[derive(Clone, Debug, PartialEq)]
pub struct Credentials {
    /// The name the device knows us by.
    pub client_id: String,
    /// Seed of our long-term signing key.
    pub seed: Vec<u8>,
    /// The device's pairing identifier.
    pub device_id: Vec<u8>,
    /// The device's long-term public key.
    pub device_key: Vec<u8>,
}

fn post(connection: &mut Connection, path: &str, body: &[u8]) -> Result<Message> {
    let request = compose(
        &format!("POST {path} HTTP/1.1"),
        &[
            ("User-Agent", "AirPlay/320.20".to_string()),
            ("Connection", "keep-alive".to_string()),
            ("X-Apple-HKP", "3".to_string()),
            ("Content-Type", "application/octet-stream".to_string()),
        ],
        body,
    );
    let response = connection.exchange(&request, TIMEOUT)?;
    if response.status() != 200 {
        return Err(fail!(
            "the device refused {path} (status {})",
            response.status()
        ));
    }
    Ok(response)
}

/// Why a device refuses a pairing step, in words.
pub fn refusal(code: u8) -> &'static str {
    match code {
        2 => "wrong code",
        3 => "too many attempts, wait before trying again",
        4 => "the device accepts no more pairings",
        5 => "too many attempts, restart the device",
        6 => "pairing is not available right now",
        7 => "the device is busy with another pairing",
        _ => "the device refused",
    }
}

/// Reads a pairing answer, turning the device's error item into a message.
fn items(response: &Message) -> Result<Vec<(u8, Vec<u8>)>> {
    let items = tlv8::decode(&response.body);
    match tlv8::get(&items, tlv8::ERROR) {
        Some(code) => Err(fail!(
            "pairing failed: {}",
            refusal(code.first().copied().unwrap_or(0))
        )),
        None => Ok(items),
    }
}

fn need(items: &[(u8, Vec<u8>)], kind: u8) -> Result<&[u8]> {
    tlv8::get(items, kind).ok_or_else(|| fail!("the device sent an incomplete pairing answer"))
}

/// Asks the device to show its pairing code.
pub fn show_code(connection: &mut Connection) -> Result<()> {
    post(connection, "/pair-pin-start", &[]).map(drop)
}

/// The first exchange of a pairing: the device's salt and public value. It must be followed by
/// [`finish_setup`] on the same connection.
pub fn begin_setup(connection: &mut Connection) -> Result<(Vec<u8>, Vec<u8>)> {
    let first = tlv8::encode(&[(tlv8::METHOD, &[0]), (tlv8::SEQUENCE, &[1])]);
    let answer = items(&post(connection, "/pair-setup", &first)?)?;
    Ok((
        need(&answer, tlv8::SALT)?.to_vec(),
        need(&answer, tlv8::PUBLIC_KEY)?.to_vec(),
    ))
}

/// Completes a pairing with the code shown on the screen.
pub fn finish_setup(
    connection: &mut Connection,
    code: &str,
    salt: &[u8],
    device_public: &[u8],
) -> Result<Credentials> {
    let client = srp::Client::new(&crypto::random::<32>()?);
    let session = client
        .session("Pair-Setup", code, salt, device_public)
        .ok_or_else(|| fail!("the device sent an invalid pairing value"))?;

    let third = tlv8::encode(&[
        (tlv8::SEQUENCE, &[3]),
        (tlv8::PUBLIC_KEY, &client.public),
        (tlv8::PROOF, &session.proof),
    ]);
    let answer = items(&post(connection, "/pair-setup", &third)?)?;
    if need(&answer, tlv8::PROOF)? != session.expected.as_slice() {
        return Err(fail!("pairing failed: the device does not know the code"));
    }

    // Exchange long-term keys, under a key only someone who knew the code can derive.
    let seed: [u8; 32] = crypto::random()?;
    let identity = Identity::from_seed(&seed)?;
    let client_id = crypto::uuid()?.to_lowercase();
    let signing = crypto::derive(
        &session.key,
        "Pair-Setup-Controller-Sign-Salt",
        "Pair-Setup-Controller-Sign-Info",
    )?;
    let message = [signing.as_slice(), client_id.as_bytes(), &identity.public()].concat();
    let inner = tlv8::encode(&[
        (tlv8::IDENTIFIER, client_id.as_bytes()),
        (tlv8::PUBLIC_KEY, &identity.public()),
        (tlv8::SIGNATURE, &identity.sign(&message)),
    ]);
    let key = crypto::derive(
        &session.key,
        "Pair-Setup-Encrypt-Salt",
        "Pair-Setup-Encrypt-Info",
    )?;
    let sealed = crypto::seal(&key, crypto::nonce(*b"PS-Msg05"), &[], &inner)?;
    let fifth = tlv8::encode(&[(tlv8::SEQUENCE, &[5]), (tlv8::ENCRYPTED_DATA, &sealed)]);
    let answer = items(&post(connection, "/pair-setup", &fifth)?)?;

    let opened = crypto::open(
        &key,
        crypto::nonce(*b"PS-Msg06"),
        &[],
        need(&answer, tlv8::ENCRYPTED_DATA)?,
    )?;
    let device = tlv8::decode(&opened);
    let device_id = need(&device, tlv8::IDENTIFIER)?.to_vec();
    let device_key = need(&device, tlv8::PUBLIC_KEY)?.to_vec();
    // The device signs with the key it gives us: it proves the key is its own.
    let accessory = crypto::derive(
        &session.key,
        "Pair-Setup-Accessory-Sign-Salt",
        "Pair-Setup-Accessory-Sign-Info",
    )?;
    let signed = [accessory.as_slice(), &device_id, &device_key].concat();
    if !crypto::verify(&device_key, &signed, need(&device, tlv8::SIGNATURE)?) {
        return Err(fail!(
            "pairing failed: the device's identity does not check out"
        ));
    }
    Ok(Credentials {
        client_id,
        seed: seed.to_vec(),
        device_id,
        device_key,
    })
}

/// Proves an existing pairing on a fresh connection and returns the secret both sides now
/// share, from which the channel keys are derived.
pub fn verify(connection: &mut Connection, credentials: &Credentials) -> Result<Vec<u8>> {
    let exchange = Exchange::new()?;
    let ours = exchange.public.clone();
    let first = tlv8::encode(&[(tlv8::SEQUENCE, &[1]), (tlv8::PUBLIC_KEY, &ours)]);
    let answer = items(&post(connection, "/pair-verify", &first)?)?;
    let theirs = need(&answer, tlv8::PUBLIC_KEY)?.to_vec();
    let shared = exchange.shared(&theirs)?;

    let key = crypto::derive(
        &shared,
        "Pair-Verify-Encrypt-Salt",
        "Pair-Verify-Encrypt-Info",
    )?;
    let opened = crypto::open(
        &key,
        crypto::nonce(*b"PV-Msg02"),
        &[],
        need(&answer, tlv8::ENCRYPTED_DATA)?,
    )
    .map_err(|_| fail!("this device no longer knows this pairing: pair again"))?;
    let device = tlv8::decode(&opened);
    let signed = [theirs.as_slice(), need(&device, tlv8::IDENTIFIER)?, &ours].concat();
    if need(&device, tlv8::IDENTIFIER)? != credentials.device_id.as_slice()
        || !crypto::verify(
            &credentials.device_key,
            &signed,
            need(&device, tlv8::SIGNATURE)?,
        )
    {
        return Err(fail!("this is not the device that was paired: pair again"));
    }

    let identity = Identity::from_seed(&credentials.seed)?;
    let message = [ours.as_slice(), credentials.client_id.as_bytes(), &theirs].concat();
    let inner = tlv8::encode(&[
        (tlv8::IDENTIFIER, credentials.client_id.as_bytes()),
        (tlv8::SIGNATURE, &identity.sign(&message)),
    ]);
    let sealed = crypto::seal(&key, crypto::nonce(*b"PV-Msg03"), &[], &inner)?;
    let third = tlv8::encode(&[(tlv8::SEQUENCE, &[3]), (tlv8::ENCRYPTED_DATA, &sealed)]);
    items(&post(connection, "/pair-verify", &third)?)
        .map_err(|_| fail!("the device no longer accepts this pairing: pair again"))?;
    Ok(shared)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_refusals() {
        assert_eq!(refusal(2), "wrong code");
        assert!(refusal(3).contains("wait"));
        for code in [4, 5, 6, 7] {
            assert_ne!(refusal(code), refusal(0));
        }
    }

    #[test]
    fn reads_pairing_answers() {
        let wrong = Message {
            start: "HTTP/1.1 200 OK".into(),
            headers: vec![],
            body: tlv8::encode(&[(tlv8::SEQUENCE, &[4]), (tlv8::ERROR, &[2])]),
        };
        assert!(items(&wrong).unwrap_err().0.contains("wrong code"));
        let fine = Message {
            start: "HTTP/1.1 200 OK".into(),
            headers: vec![],
            body: tlv8::encode(&[(tlv8::SALT, &[1, 2])]),
        };
        let read = items(&fine).unwrap();
        assert_eq!(need(&read, tlv8::SALT).unwrap(), [1, 2]);
        assert!(need(&read, tlv8::PROOF).is_err());
    }
}
