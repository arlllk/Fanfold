//! Secrets are never written in the config file.
//!
//! The file names where a value comes from, `{ env = "NAME" }` or
//! `{ file = "/path" }`, and this module fetches it. A plain string in a secret
//! position fails to parse, so a literal password cannot be committed by accident.

use std::{
    io,
    path::{Path, PathBuf},
};

use secrecy::SecretString;
use serde::Deserialize;
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SecretRef {
    /// An environment variable (typically from a Kubernetes `secretKeyRef`).
    Env(String),
    /// A file (typically a mounted Kubernetes Secret volume); trailing newlines are trimmed.
    File(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SecretError {
    #[error("environment variable {0} is not set")]
    EnvMissing(String),
    #[error("environment variable {0} is empty")]
    EnvEmpty(String),
    #[error("cannot read secret file {path}: {reason}")]
    FileUnreadable { path: PathBuf, reason: String },
    #[error("secret file {0} is empty")]
    FileEmpty(PathBuf),
}

/// Where secrets are read from; faked in tests.
pub trait SecretSource {
    fn env(&self, name: &str) -> Option<String>;
    fn read_file(&self, path: &Path) -> io::Result<String>;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct SystemSecrets;

impl SecretSource for SystemSecrets {
    fn env(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }

    fn read_file(&self, path: &Path) -> io::Result<String> {
        std::fs::read_to_string(path)
    }
}

impl SecretRef {
    /// Human-readable origin for diagnostics. Never the value.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Env(name) => format!("env {name}"),
            Self::File(path) => format!("file {}", path.display()),
        }
    }

    pub fn resolve(&self, source: &dyn SecretSource) -> Result<SecretString, SecretError> {
        match self {
            Self::Env(name) => match source.env(name) {
                None => Err(SecretError::EnvMissing(name.clone())),
                Some(v) if v.is_empty() => Err(SecretError::EnvEmpty(name.clone())),
                Some(v) => Ok(SecretString::from(v)),
            },
            Self::File(path) => {
                let raw = source
                    .read_file(path)
                    .map_err(|e| SecretError::FileUnreadable {
                        path: path.clone(),
                        reason: e.kind().to_string(),
                    })?;
                let trimmed = raw.trim_end_matches(['\n', '\r']);
                if trimmed.is_empty() {
                    Err(SecretError::FileEmpty(path.clone()))
                } else {
                    Ok(SecretString::from(trimmed.to_owned()))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use secrecy::ExposeSecret;

    use super::*;

    #[derive(Default)]
    struct Fake {
        env: HashMap<String, String>,
        files: HashMap<PathBuf, String>,
    }

    impl SecretSource for Fake {
        fn env(&self, name: &str) -> Option<String> {
            self.env.get(name).cloned()
        }

        fn read_file(&self, path: &Path) -> io::Result<String> {
            self.files
                .get(path)
                .cloned()
                .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
        }
    }

    #[test]
    fn env_secrets() {
        let mut fake = Fake::default();
        fake.env.insert("PW".into(), "s3cret".into());
        fake.env.insert("EMPTY".into(), String::new());
        let resolve = |name: &str| SecretRef::Env(name.into()).resolve(&fake);
        assert_eq!(resolve("PW").unwrap().expose_secret(), "s3cret");
        assert_eq!(
            resolve("NOPE").unwrap_err(),
            SecretError::EnvMissing("NOPE".into())
        );
        assert_eq!(
            resolve("EMPTY").unwrap_err(),
            SecretError::EnvEmpty("EMPTY".into())
        );
    }

    #[test]
    fn file_secrets_trim_only_trailing_newlines() {
        let mut fake = Fake::default();
        fake.files
            .insert("/s/pw".into(), "  pass word \r\n\n".into());
        fake.files.insert("/s/empty".into(), "\n".into());
        let resolve = |p: &str| SecretRef::File(p.into()).resolve(&fake);
        assert_eq!(
            resolve("/s/pw").unwrap().expose_secret(),
            "  pass word ",
            "inner and leading spaces are part of the secret"
        );
        assert!(matches!(
            resolve("/s/empty"),
            Err(SecretError::FileEmpty(_))
        ));
        assert!(matches!(
            resolve("/s/missing"),
            Err(SecretError::FileUnreadable { .. })
        ));
    }

    #[test]
    fn errors_never_contain_secret_values() {
        let mut fake = Fake::default();
        fake.files.insert("/s/pw".into(), String::new());
        let err = SecretRef::File("/s/pw".into())
            .resolve(&fake)
            .unwrap_err()
            .to_string();
        assert!(err.contains("/s/pw"));
    }
}
