use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Metadata {
    pub format: u32,
    pub package: PackageMetadata,
    pub base: BaseMetadata,
    pub build: BuildMetadata,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackageMetadata {
    pub installable: String,
    pub flake_ref: String,
    pub attribute: String,
    pub system: String,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pname: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BaseMetadata {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nixpkgs_revision: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nixpkgs_last_modified: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nixpkgs_locked_nar_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nixpkgs_resolved_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nixpkgs_path: Option<String>,

    pub derivation: String,
    pub output: String,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_revision: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_ca: Option<String>,

    pub post_patch_source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub post_patch_source_hash: Option<String>,

    pub git_commit: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildMetadata {
    pub strategy: String,
    pub patch_handling: String,
}

impl Metadata {
    pub fn read(path: &Path) -> Result<Self> {
        let text = fs::read_to_string(path)
            .with_context(|| format!("failed to read metadata {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("failed to parse {}", path.display()))
    }

    pub fn write(&self, path: &Path) -> Result<()> {
        let text = toml::to_string_pretty(self).context("failed to serialize metadata")?;
        fs::write(path, text).with_context(|| format!("failed to write {}", path.display()))
    }
}
