use std::{fmt, str::FromStr};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

/// The prefix of a signing key, as Standard Webhooks writes a symmetric one.
const PREFIX: &str = "whsec_";

/// `--signing-key`: the keys that sign the callbacks, comma separated (several: a rotation).
/// Each is `whsec_` and the base64 of 24 to 64 random bytes: `openssl rand -base64 32`.
#[derive(Clone, PartialEq, Eq)]
pub struct SigningKeys(Vec<Vec<u8>>);

impl SigningKeys {
    /// The value of the signature header for a delivery: `v1,<base64>` for every key, separated
    /// by a space. The signed text is `{key}.{timestamp}.{body}`: the key of the callback job,
    /// the time as it is in the header, the body as it is sent.
    pub fn sign(&self, job_key: &str, timestamp: i64, body: &[u8]) -> String {
        self.0
            .iter()
            .map(|secret| {
                let mut mac =
                    Hmac::<Sha256>::new_from_slice(secret).expect("HMAC takes a key of any length");
                mac.update(job_key.as_bytes());
                mac.update(b".");
                mac.update(timestamp.to_string().as_bytes());
                mac.update(b".");
                mac.update(body);
                format!("v1,{}", STANDARD.encode(mac.finalize().into_bytes()))
            })
            .collect::<Vec<_>>()
            .join(" ")
    }
}

impl FromStr for SigningKeys {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let keys = value
            .split(',')
            .map(str::trim)
            .filter(|key| !key.is_empty())
            .map(|key| {
                let bytes = key
                    .strip_prefix(PREFIX)
                    .and_then(|encoded| STANDARD.decode(encoded).ok())
                    .filter(|bytes| (24..=64).contains(&bytes.len()));
                // the value stays out of the message: it is a secret
                bytes.ok_or_else(|| {
                    format!("a signing key is {PREFIX} and the base64 of 24 to 64 bytes: openssl rand -base64 32")
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        if keys.is_empty() {
            return Err("at least one key is required".to_owned());
        }
        Ok(SigningKeys(keys))
    }
}

/// The values stay out of the logs.
impl fmt::Debug for SigningKeys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SigningKeys({} keys)", self.0.len())
    }
}

#[test]
fn the_signature_of_the_standard() {
    // the example of the Standard Webhooks libraries
    let keys: SigningKeys = "whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw"
        .parse()
        .expect("key");
    assert_eq!(
        keys.sign(
            "msg_p5jXN8AQM9LWM0D4loKWxJek",
            1_614_265_330,
            br#"{"test": 2432232314}"#
        ),
        "v1,g0hM9SsE+OTPJTGt/tmIKtSyZlE3uFJELVlNIOLJ1OE="
    );
    // a rotation: one signature per key, the first key first
    let two: SigningKeys =
        "whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw, whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw"
            .parse()
            .expect("keys");
    assert_eq!(two.sign("a", 1, b"b").split(' ').count(), 2);
}

#[test]
fn a_key_is_checked() {
    for bad in [
        "",
        " , ",
        "short",
        "whsec_c2hvcnQ=",
        "MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw",
    ] {
        let err = bad.parse::<SigningKeys>().expect_err(bad);
        assert!(!err.contains(bad) || bad.is_empty(), "{err}");
    }
}
