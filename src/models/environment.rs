use std::{fmt, str::FromStr};

/// Where irisqo runs: `--env` or `IRISQO_ENV`. Production unless told otherwise, so an
/// instance with no settings gets the safe behavior.
///
/// | | development | staging, production |
/// |---|---|---|
/// | `/echo`, `/error` | served | not found |
/// | private and loopback targets | allowed | refused, unless in `--allow-targets` |
///
/// Staging behaves as production; the name also goes to the telemetry
/// (`deployment.environment.name`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Environment {
    Development,
    Staging,
    #[default]
    Production,
}

impl Environment {
    pub const fn as_str(self) -> &'static str {
        match self {
            Environment::Development => "development",
            Environment::Staging => "staging",
            Environment::Production => "production",
        }
    }

    pub const fn is_development(self) -> bool {
        matches!(self, Environment::Development)
    }
}

impl FromStr for Environment {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.to_ascii_lowercase().as_str() {
            "development" | "dev" => Ok(Environment::Development),
            "staging" | "stage" => Ok(Environment::Staging),
            "production" | "prod" => Ok(Environment::Production),
            _ => Err(format!(
                "unknown environment {value:?}: development, staging or production"
            )),
        }
    }
}

impl fmt::Display for Environment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[test]
fn environment_names() {
    assert_eq!("Dev".parse(), Ok(Environment::Development));
    assert_eq!("staging".parse(), Ok(Environment::Staging));
    assert_eq!("PROD".parse(), Ok(Environment::Production));
    assert!("test".parse::<Environment>().is_err());
    assert_eq!(Environment::default(), Environment::Production);
}
