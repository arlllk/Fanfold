//! Command line: `fanfold [--config PATH] [run | check-config | enqueue URL]`.

use std::path::PathBuf;

use thiserror::Error;

pub const DEFAULT_CONFIG: &str = "/etc/fanfold/config.toml";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Run the worker (the default).
    Run,
    /// Validate the config and resolve every secret, then exit.
    CheckConfig,
    /// Queue one AO3 work URL.
    Enqueue(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cli {
    pub config_path: PathBuf,
    pub command: Command,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum UsageError {
    #[error("{0}")]
    Message(String),
}

pub const USAGE: &str = "usage: fanfold [--config PATH] [run | check-config | enqueue URL]\n\
    \n  config path: --config, else $FANFOLD_CONFIG, else /etc/fanfold/config.toml\n";

/// Parse arguments (without the program name). `env_config` is `$FANFOLD_CONFIG`.
pub fn parse_args(args: &[String], env_config: Option<&str>) -> Result<Cli, UsageError> {
    let mut config_path = env_config.filter(|p| !p.is_empty()).map(PathBuf::from);
    let mut rest = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--config" | "-c" => {
                let value = iter
                    .next()
                    .ok_or_else(|| UsageError::Message("--config needs a path".into()))?;
                config_path = Some(PathBuf::from(value));
            }
            flag if flag.starts_with('-') => {
                return Err(UsageError::Message(format!("unknown option {flag}")));
            }
            _ => rest.push(arg.as_str()),
        }
    }
    let command = match rest.as_slice() {
        [] | ["run"] => Command::Run,
        ["check-config"] => Command::CheckConfig,
        ["enqueue", url] => Command::Enqueue((*url).to_owned()),
        ["enqueue"] => return Err(UsageError::Message("enqueue needs a URL".into())),
        other => {
            return Err(UsageError::Message(format!(
                "unrecognised command: {}",
                other.join(" ")
            )));
        }
    };
    Ok(Cli {
        config_path: config_path.unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG)),
        command,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    fn parse(list: &[&str]) -> Result<Cli, UsageError> {
        parse_args(&args(list), None)
    }

    #[test]
    fn defaults() {
        assert_eq!(
            parse(&[]).unwrap(),
            Cli {
                config_path: DEFAULT_CONFIG.into(),
                command: Command::Run
            }
        );
        assert_eq!(parse(&["run"]).unwrap().command, Command::Run);
    }

    #[test]
    fn commands() {
        assert_eq!(
            parse(&["check-config"]).unwrap().command,
            Command::CheckConfig
        );
        assert_eq!(
            parse(&["enqueue", "https://archiveofourown.org/works/1"])
                .unwrap()
                .command,
            Command::Enqueue("https://archiveofourown.org/works/1".into())
        );
    }

    #[test]
    fn config_path_precedence_is_flag_then_env_then_default() {
        let with_env = |a: &[&str]| {
            parse_args(&args(a), Some("/from/env.toml"))
                .unwrap()
                .config_path
        };
        assert_eq!(with_env(&[]), PathBuf::from("/from/env.toml"));
        assert_eq!(
            with_env(&["--config", "/flag.toml"]),
            PathBuf::from("/flag.toml")
        );
        assert_eq!(
            with_env(&["run", "-c", "/flag.toml"]),
            PathBuf::from("/flag.toml")
        );
        assert_eq!(
            parse_args(&[], Some("")).unwrap().config_path,
            PathBuf::from(DEFAULT_CONFIG),
            "empty env is ignored"
        );
    }

    #[test]
    fn bad_usage() {
        for bad in [
            &["--config"][..],
            &["enqueue"],
            &["--bogus"],
            &["frobnicate"],
            &["run", "extra"],
            &["enqueue", "a", "b"],
        ] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
    }
}
