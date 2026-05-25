use cargo_subcommand::Error as SubcommandError;
use rndk::error::NdkError;
use serde_json::Error as JsonError;
use std::io::Error as IoError;
use thiserror::Error;
use toml::de::Error as TomlError;

#[derive(Debug, Error)]
pub enum Error {
    #[error(transparent)]
    Subcommand(#[from] SubcommandError),
    #[error("Failed to parse config: {0}")]
    Config(#[from] TomlError),
    #[error("Manifest `{0}` must contain a `[package]` table")]
    MissingPackageTable(std::path::PathBuf),
    #[error(transparent)]
    Ndk(#[from] NdkError),
    #[error(transparent)]
    Io(#[from] IoError),
    #[error("`cargo metadata` failed: {0}")]
    MetadataCommandFailed(String),
    #[error("Failed to parse `cargo metadata` output")]
    MetadataJson(#[from] JsonError),
    #[error("Configure a release keystore via `[package.metadata.android.signing.{0}]`")]
    MissingReleaseKey(String),
    #[error("Set the keystore password via CARGO_RAPK_{0}_KEYSTORE_PASSWORD")]
    MissingKeystorePassword(String),
    #[error("`workspace=false` is unsupported")]
    InheritedFalse,
    #[error("`workspace=true` requires a workspace")]
    InheritanceMissingWorkspace,
    #[error("Workspace root manifest has no `[workspace]` table")]
    MissingWorkspaceTable,
    #[error("Failed to inherit field: `workspace.{0}` was not defined in workspace root manifest")]
    WorkspaceMissingInheritedField(&'static str),
    #[error("`version_name` must not be set manually; it is derived from the package version")]
    VersionNameSet,
    #[error("`version_code` must not be set manually; it is derived from the package version")]
    VersionCodeSet,
}

impl Error {
    pub fn invalid_args() -> Self {
        Self::Subcommand(SubcommandError::InvalidArgs)
    }
}
