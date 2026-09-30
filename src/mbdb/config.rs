//! Configuration for the mbdb import and query CLIs.
//!
//! Loads defaults from `config.toml` in the repository root (or an explicit
//! `--config` path). CLI flags always take precedence over config-file
//! values, which in turn take precedence over the hardcoded defaults pinned
//! in this module.
//!
//! The config file is fully optional — the CLIs work with flags alone.

use std::fs;
use std::path::Path;

use serde::Deserialize;
use std::str::FromStr;

/// Default dump directory, mirroring the shipped `config.toml` (plan §6).
///
/// A leading `~/` is expanded to the user's home directory on use.
pub const DEFAULT_DUMP_DIR: &str = "~/musicbrainz_db/dumps";

/// Default path to the output DuckDB database file, mirroring plan §6.
pub const DEFAULT_DB: &str = "~/musicbrainz_db/musicbrainz.duckdb";

/// Default import scope, mirroring the shipped `config.toml` (plan §6).
pub const DEFAULT_TABLES: &str = "core+derived";

/// Path to the default config file in the repository root.
pub const DEFAULT_CONFIG_PATH: &str = "config.toml";

/// Expand a leading `~/` in a path string to the user's home directory.
///
/// Returns the original string unchanged when there is no `~/` prefix or the
/// home directory cannot be determined.
pub fn expand_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        let mut expanded = std::path::PathBuf::from(home);
        expanded.push(rest);
        return expanded.to_string_lossy().into_owned();
    }
    path.to_owned()
}

/// Raw values from `config.toml`.
///
/// Every field is optional — a missing field falls back to the corresponding
/// CLI flag or the hardcoded default.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Directory where MusicBrainz dump tarballs are downloaded and kept.
    pub dump_dir: Option<String>,

    /// Path to the output DuckDB database file.
    pub db: Option<String>,

    /// Import scope: which table groups to load.
    pub tables: Option<String>,
}

impl Config {
    /// Load configuration from a TOML file.
    ///
    /// Returns `Ok(None)` if the file does not exist (the config file is
    /// optional). Returns an error if the file exists but is malformed.
    pub fn load(path: impl AsRef<Path>) -> Result<Option<Self>, ConfigError> {
        let path = path.as_ref();
        if !path.exists() {
            return Ok(None);
        }
        let content = fs::read_to_string(path).map_err(|e| ConfigError::Io {
            path: path.to_owned(),
            source: e,
        })?;
        let config: Config = toml::from_str(&content).map_err(|e| ConfigError::Parse {
            path: path.to_owned(),
            source: e,
        })?;
        Ok(Some(config))
    }

    /// Load from the default `config.toml` in the current directory.
    pub fn load_default() -> Result<Option<Self>, ConfigError> {
        let path = Path::new(DEFAULT_CONFIG_PATH);
        Self::load(path)
    }
}

/// Import scope: which table groups to load.
///
/// Mirrors the `tables` key in `config.toml` and plan §5/§6
/// (`"core"` | `"derived"` | `"core+derived"`).
#[derive(Debug, PartialEq, Eq, Copy, Clone, Hash)]
pub enum Tables {
    /// Core `mbdump` tables only.
    Core,

    /// `mbdump-derived` tables only.
    Derived,

    /// Both table groups (the default).
    CoreAndDerived,
}

impl FromStr for Tables {
    type Err = ConfigError;

    /// Parse a scope from a `tables` config or CLI value.
    fn from_str(s: &str) -> Result<Self, ConfigError> {
        match s {
            "core" => Ok(Tables::Core),
            "derived" => Ok(Tables::Derived),
            "core+derived" => Ok(Tables::CoreAndDerived),
            _ => Err(ConfigError::InvalidTables {
                value: s.to_owned(),
            }),
        }
    }
}

/// CLI flag values shared by the import and query CLIs that override config.
///
/// All fields are `Option`al — an unset flag defers to the config-file value,
/// then to the hardcoded default.
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct CliOverrides {
    /// Override for `dump_dir` (import CLI).
    pub dump_dir: Option<String>,

    /// Override for `db` (import and query CLIs).
    pub db: Option<String>,

    /// Override for `tables` (import CLI).
    pub tables: Option<String>,
}

impl CliOverrides {
    /// Overrides with no flags set — pure config/default resolution.
    pub fn empty() -> Self {
        Self {
            dump_dir: None,
            db: None,
            tables: None,
        }
    }
}

/// Effective settings after merging CLI overrides and config-file values.
///
/// Precedence: CLI flag > `config.toml` value > hardcoded default. Path
/// fields have a leading `~/` expanded to the user's home directory.
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct Settings {
    /// Directory where MusicBrainz dump tarballs live.
    pub dump_dir: String,

    /// Path to the output DuckDB database file.
    pub db: String,

    /// Import scope: which table groups to load.
    pub tables: Tables,
}

impl Settings {
    /// Resolve the effective settings from an optional parsed config file and
    /// the CLI flag overrides.
    ///
    /// Fails when the resolved `tables` value is not one of `core`,
    /// `derived`, or `core+derived`.
    pub fn resolve(config: Option<&Config>, cli: &CliOverrides) -> Result<Self, ConfigError> {
        let dump_dir = cli
            .dump_dir
            .clone()
            .or_else(|| config.and_then(|c| c.dump_dir.clone()))
            .unwrap_or_else(|| DEFAULT_DUMP_DIR.to_owned());
        let db = cli
            .db
            .clone()
            .or_else(|| config.and_then(|c| c.db.clone()))
            .unwrap_or_else(|| DEFAULT_DB.to_owned());
        let tables = cli
            .tables
            .clone()
            .or_else(|| config.and_then(|c| c.tables.clone()))
            .unwrap_or_else(|| DEFAULT_TABLES.to_owned());

        let tables = tables.parse::<Tables>()?;

        Ok(Self {
            dump_dir: expand_tilde(&dump_dir),
            db: expand_tilde(&db),
            tables,
        })
    }
}

/// Errors that can occur when loading or resolving configuration.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The config file could not be read.
    #[error("failed to read config file `{path}`: {source}")]
    Io {
        /// Path to the config file.
        path: std::path::PathBuf,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// The config file contains invalid TOML.
    #[error("failed to parse config file `{path}`: {source}")]
    Parse {
        /// Path to the config file.
        path: std::path::PathBuf,
        /// The underlying TOML parse error.
        #[source]
        source: toml::de::Error,
    },

    /// The `tables` value is not a recognized import scope.
    #[error("invalid tables value `{value}` — expected `core`, `derived`, or `core+derived`")]
    InvalidTables {
        /// The offending value.
        value: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Write a config file into a fresh temp dir and return the dir handle
    /// (kept alive for the lifetime of the caller) plus the file path.
    fn sample_config_file(contents: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, contents).unwrap();
        (dir, path)
    }

    #[test]
    fn test_expand_tilde_plain_path_is_unchanged() {
        assert_eq!(expand_tilde("data/dumps"), "data/dumps");
    }

    #[test]
    fn test_expand_tilde_home_prefix() {
        let expected = match std::env::var_os("HOME") {
            Some(home) => {
                let mut expanded = std::path::PathBuf::from(home);
                expanded.push("musicbrainz_db/dumps");
                expanded.to_string_lossy().into_owned()
            }
            // No HOME in the environment: the `~/` prefix is left as-is.
            None => "~/musicbrainz_db/dumps".to_owned(),
        };
        assert_eq!(expand_tilde("~/musicbrainz_db/dumps"), expected);
    }

    #[test]
    fn test_load_missing_file_returns_none() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        let result = Config::load(&path).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_load_valid_config() {
        let (_dir, path) = sample_config_file(
            r#"
dump_dir = "/data/dumps"
db = "/data/musicbrainz.duckdb"
tables = "core"
"#,
        );
        let config = Config::load(&path).unwrap().unwrap();
        assert_eq!(config.dump_dir.as_deref(), Some("/data/dumps"));
        assert_eq!(config.db.as_deref(), Some("/data/musicbrainz.duckdb"));
        assert_eq!(config.tables.as_deref(), Some("core"));
    }

    #[test]
    fn test_load_minimal_config() {
        let (_dir, path) = sample_config_file("# no keys set\n");
        let config = Config::load(&path).unwrap().unwrap();
        assert!(config.dump_dir.is_none());
        assert!(config.db.is_none());
        assert!(config.tables.is_none());
    }

    #[test]
    fn test_load_explicit_empty_value_is_preserved() {
        let (_dir, path) = sample_config_file("db = \"\"\n");
        let config = Config::load(&path).unwrap().unwrap();
        assert_eq!(config.db.as_deref(), Some(""));
    }

    #[test]
    fn test_load_malformed_toml_returns_error() {
        let (_dir, path) = sample_config_file("[[[invalid]]]\n");
        let result = Config::load(&path);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ConfigError::Parse { .. }));
    }

    #[test]
    fn test_load_unknown_key_returns_error() {
        let (_dir, path) = sample_config_file("unknown_key = \"x\"\n");
        let result = Config::load(&path);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ConfigError::Parse { .. }));
    }

    #[test]
    fn test_load_default_config_smoke() {
        // `cargo test` runs from the crate root, where the shipped
        // `config.toml` lives; it must parse against the `Config` shape.
        let config = Config::load_default()
            .unwrap()
            .expect("repo config.toml missing");
        assert_eq!(config.dump_dir.as_deref(), Some("~/musicbrainz_db/dumps"));
        assert_eq!(
            config.db.as_deref(),
            Some("~/musicbrainz_db/musicbrainz.duckdb")
        );
        assert_eq!(config.tables.as_deref(), Some("core+derived"));
    }

    #[test]
    fn test_tables_from_str_known_scopes() {
        assert_eq!(Tables::from_str("core").unwrap(), Tables::Core);
        assert_eq!(Tables::from_str("derived").unwrap(), Tables::Derived);
        assert_eq!(
            Tables::from_str("core+derived").unwrap(),
            Tables::CoreAndDerived
        );
    }

    #[test]
    fn test_tables_from_str_invalid() {
        let result = Tables::from_str("all");
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            ConfigError::InvalidTables { .. }
        ));
    }

    #[test]
    fn test_resolve_defaults_without_config_or_cli() {
        let settings = Settings::resolve(None, &CliOverrides::empty()).unwrap();
        assert_eq!(settings.dump_dir, expand_tilde(DEFAULT_DUMP_DIR));
        assert_eq!(settings.db, expand_tilde(DEFAULT_DB));
        assert_eq!(settings.tables, Tables::CoreAndDerived);
    }

    #[test]
    fn test_resolve_config_values() {
        let (_dir, path) = sample_config_file(
            r#"
dump_dir = "/data/dumps"
db = "/data/musicbrainz.duckdb"
tables = "derived"
"#,
        );
        let config = Config::load(&path).unwrap().unwrap();
        let settings = Settings::resolve(Some(&config), &CliOverrides::empty()).unwrap();
        assert_eq!(settings.dump_dir, "/data/dumps");
        assert_eq!(settings.db, "/data/musicbrainz.duckdb");
        assert_eq!(settings.tables, Tables::Derived);
    }

    #[test]
    fn test_resolve_cli_overrides_config() {
        let (_dir, path) = sample_config_file(
            r#"
dump_dir = "/data/dumps"
db = "/data/musicbrainz.duckdb"
tables = "core"
"#,
        );
        let config = Config::load(&path).unwrap().unwrap();
        let cli = CliOverrides {
            dump_dir: Some("/cli/dumps".to_string()),
            db: Some("/cli/musicbrainz.duckdb".to_string()),
            tables: Some("core+derived".to_string()),
        };
        let settings = Settings::resolve(Some(&config), &cli).unwrap();
        assert_eq!(settings.dump_dir, "/cli/dumps");
        assert_eq!(settings.db, "/cli/musicbrainz.duckdb");
        assert_eq!(settings.tables, Tables::CoreAndDerived);
    }

    #[test]
    fn test_resolve_partial_cli_falls_back_to_config() {
        let (_dir, path) = sample_config_file(
            r#"
dump_dir = "/data/dumps"
db = "/data/musicbrainz.duckdb"
tables = "derived"
"#,
        );
        let config = Config::load(&path).unwrap().unwrap();
        let cli = CliOverrides {
            dump_dir: None,
            db: Some("/cli/musicbrainz.duckdb".to_string()),
            tables: None,
        };
        let settings = Settings::resolve(Some(&config), &cli).unwrap();
        assert_eq!(settings.dump_dir, "/data/dumps");
        assert_eq!(settings.db, "/cli/musicbrainz.duckdb");
        assert_eq!(settings.tables, Tables::Derived);
    }

    #[test]
    fn test_resolve_invalid_tables_in_config() {
        let (_dir, path) = sample_config_file("tables = \"all\"\n");
        let config = Config::load(&path).unwrap().unwrap();
        let result = Settings::resolve(Some(&config), &CliOverrides::empty());
        assert!(matches!(
            result.unwrap_err(),
            ConfigError::InvalidTables { .. }
        ));
    }

    #[test]
    fn test_resolve_invalid_tables_in_cli() {
        let cli = CliOverrides {
            dump_dir: None,
            db: None,
            tables: Some("everything".to_string()),
        };
        let result = Settings::resolve(None, &cli);
        assert!(matches!(
            result.unwrap_err(),
            ConfigError::InvalidTables { .. }
        ));
    }

    #[test]
    fn test_resolve_expands_tilde() {
        let (_dir, path) = sample_config_file(
            r#"
dump_dir = "~/custom/dumps"
db = "~/custom/db.duckdb"
"#,
        );
        let config = Config::load(&path).unwrap().unwrap();
        let settings = Settings::resolve(Some(&config), &CliOverrides::empty()).unwrap();
        assert_eq!(settings.dump_dir, expand_tilde("~/custom/dumps"));
        assert_eq!(settings.db, expand_tilde("~/custom/db.duckdb"));
        assert_eq!(settings.tables, Tables::CoreAndDerived);
    }
}
