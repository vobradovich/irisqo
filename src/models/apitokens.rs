use std::{fmt, str::FromStr};

/// `--api-token`: the tokens that open the API, comma separated, so a new one can be rolled
/// out before the old one is removed. None: the API is open.
#[derive(Clone, PartialEq, Eq, Default)]
pub struct ApiTokens(Vec<String>);

/// Shorter tokens are refused: they could be guessed.
pub const MIN_TOKEN_LEN: usize = 16;

impl ApiTokens {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// One of the tokens, compared in constant time (no early exit on the first byte that
    /// differs, which would tell an attacker how much of a guess is right).
    #[must_use = "an unchecked token lets every request in"]
    pub fn accepts(&self, candidate: &str) -> bool {
        self.0.iter().fold(false, |found, token| {
            found | constant_time_eq(token.as_bytes(), candidate.as_bytes())
        })
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |diff, (x, y)| diff | (x ^ y)) == 0
}

impl FromStr for ApiTokens {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let tokens: Vec<String> = value
            .split(',')
            .map(str::trim)
            .filter(|token| !token.is_empty())
            .map(str::to_owned)
            .collect();
        // a blank value must not leave the API open while it looks configured
        if tokens.is_empty() {
            return Err("at least one token is required".to_owned());
        }
        if tokens.iter().any(|token| token.len() < MIN_TOKEN_LEN) {
            return Err(format!(
                "a token needs at least {MIN_TOKEN_LEN} characters: e.g. openssl rand -hex 32"
            ));
        }
        Ok(ApiTokens(tokens))
    }
}

/// The values stay out of the logs.
impl fmt::Debug for ApiTokens {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ApiTokens({} tokens)", self.0.len())
    }
}

#[test]
fn tokens() {
    let tokens: ApiTokens = "0123456789abcdef, fedcba9876543210"
        .parse()
        .expect("tokens");
    assert!(tokens.accepts("0123456789abcdef"));
    assert!(tokens.accepts("fedcba9876543210"));
    assert!(!tokens.accepts("0123456789abcdeF"));
    assert!(!tokens.accepts("0123456789abcde"));
    assert!(!tokens.accepts(""));
    assert_eq!(format!("{tokens:?}"), "ApiTokens(2 tokens)");
    assert!("short".parse::<ApiTokens>().is_err());
    assert!("".parse::<ApiTokens>().is_err() && " , ".parse::<ApiTokens>().is_err());
    assert!(ApiTokens::default().is_empty() && !ApiTokens::default().accepts(""));
}
