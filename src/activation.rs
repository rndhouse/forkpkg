use std::env;
use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::metadata::Metadata;
use crate::workspace::{self, Workspace};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActivationRecord {
    pub format: u32,
    pub mode: String,
    pub package: String,
    pub installable: String,
    pub workspace: PathBuf,
    pub source: PathBuf,
    pub build_output: PathBuf,
    pub activated_at_unix: u64,
    pub links: Vec<LinkRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinkRecord {
    pub name: String,
    pub link: PathBuf,
    pub target: PathBuf,
    pub previous: PreviousLink,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PreviousLink {
    Absent,
    BackedUp { backup: PathBuf },
}

pub fn enable_path_shim(
    metadata: &Metadata,
    workspace: &Workspace,
    build_output: &Path,
) -> Result<ActivationRecord> {
    let package = package_key(metadata);
    let activation_dir = activation_dir(&package);
    let record_path = activation_dir.join("activation.toml");
    if record_path.exists() {
        bail!("{} is already active; run forkpkg disable first", package);
    }

    let bin_dir = build_output.join("bin");
    if !bin_dir.is_dir() {
        bail!("build output has no bin directory: {}", bin_dir.display());
    }

    let entries = executable_entries(&bin_dir)?;
    if entries.is_empty() {
        bail!(
            "build output has no executable entries in {}",
            bin_dir.display()
        );
    }

    let user_bin = user_bin_dir()?;
    fs::create_dir_all(&user_bin)
        .with_context(|| format!("failed to create {}", user_bin.display()))?;
    fs::create_dir_all(activation_dir.join("backups"))
        .with_context(|| format!("failed to create {}", activation_dir.display()))?;

    let mut links = Vec::new();
    for target in entries {
        let name = target
            .file_name()
            .ok_or_else(|| anyhow!("output entry has no file name: {}", target.display()))?
            .to_string_lossy()
            .into_owned();
        let link = user_bin.join(&name);
        let backup = activation_dir.join("backups").join(&name);

        let previous = if link_exists(&link)? {
            if link.is_dir() && !link.is_symlink() {
                bail!(
                    "refusing to replace existing directory in PATH: {}",
                    link.display()
                );
            }
            fs::rename(&link, &backup).with_context(|| {
                format!(
                    "failed to move existing {} to {}",
                    link.display(),
                    backup.display()
                )
            })?;
            PreviousLink::BackedUp { backup }
        } else {
            PreviousLink::Absent
        };

        symlink(&target, &link).with_context(|| {
            format!("failed to link {} to {}", link.display(), target.display())
        })?;

        links.push(LinkRecord {
            name,
            link,
            target,
            previous,
        });
    }

    let record = ActivationRecord {
        format: 1,
        mode: "path-shim".to_owned(),
        package,
        installable: metadata.package.installable.clone(),
        workspace: workspace.root.clone(),
        source: workspace.source.clone(),
        build_output: build_output.to_path_buf(),
        activated_at_unix: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("system time is before Unix epoch")?
            .as_secs(),
        links,
    };

    write_record(&record)?;
    Ok(record)
}

pub fn disable(metadata: &Metadata) -> Result<ActivationRecord> {
    let package = package_key(metadata);
    let record = read_record_by_package(&package)?;

    for link in &record.links {
        ensure_link_points_to_target(link)?;
    }

    for link in &record.links {
        if link_exists(&link.link)? {
            fs::remove_file(&link.link)
                .with_context(|| format!("failed to remove {}", link.link.display()))?;
        }

        if let PreviousLink::BackedUp { backup } = &link.previous {
            fs::rename(backup, &link.link).with_context(|| {
                format!(
                    "failed to restore {} to {}",
                    backup.display(),
                    link.link.display()
                )
            })?;
        }
    }

    let dir = activation_dir(&package);
    let record_path = dir.join("activation.toml");
    fs::remove_file(&record_path)
        .with_context(|| format!("failed to remove {}", record_path.display()))?;
    remove_dir_if_empty(&dir.join("backups"))?;
    remove_dir_if_empty(&dir)?;

    Ok(record)
}

pub fn status(metadata: &Metadata) -> Result<Option<ActivationRecord>> {
    let package = package_key(metadata);
    let record_path = activation_dir(&package).join("activation.toml");
    if !record_path.is_file() {
        return Ok(None);
    }
    read_record(&record_path).map(Some)
}

pub fn package_key(metadata: &Metadata) -> String {
    workspace::sanitize_workspace_name(
        metadata
            .package
            .pname
            .as_deref()
            .or(metadata.package.name.as_deref())
            .unwrap_or(&metadata.package.attribute),
    )
}

pub fn path_hint() -> Result<Option<String>> {
    let user_bin = user_bin_dir()?;
    let user_bin = user_bin
        .to_str()
        .ok_or_else(|| anyhow!("user bin path is not valid UTF-8"))?;
    let path = env::var("PATH").unwrap_or_default();
    let present = env::split_paths(&path).any(|entry| entry == Path::new(user_bin));
    Ok(if present {
        None
    } else {
        Some(format!("{user_bin} is not currently in PATH"))
    })
}

fn executable_entries(bin_dir: &Path) -> Result<Vec<PathBuf>> {
    let mut entries = Vec::new();
    for entry in
        fs::read_dir(bin_dir).with_context(|| format!("failed to read {}", bin_dir.display()))?
    {
        let entry =
            entry.with_context(|| format!("failed to read entry in {}", bin_dir.display()))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .with_context(|| format!("failed to inspect {}", path.display()))?;
        if file_type.is_file() || file_type.is_symlink() {
            entries.push(path);
        }
    }
    entries.sort();
    Ok(entries)
}

fn write_record(record: &ActivationRecord) -> Result<()> {
    let dir = activation_dir(&record.package);
    fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
    let path = dir.join("activation.toml");
    let text = toml::to_string_pretty(record).context("failed to serialize activation record")?;
    fs::write(&path, text).with_context(|| format!("failed to write {}", path.display()))
}

fn read_record_by_package(package: &str) -> Result<ActivationRecord> {
    let path = activation_dir(package).join("activation.toml");
    read_record(&path)
}

fn read_record(path: &Path) -> Result<ActivationRecord> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("failed to read activation record {}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("failed to parse {}", path.display()))
}

fn ensure_link_points_to_target(link: &LinkRecord) -> Result<()> {
    let actual = fs::read_link(&link.link)
        .with_context(|| format!("{} is not the symlink forkpkg created", link.link.display()))?;
    if actual != link.target {
        bail!(
            "refusing to disable because {} now points to {}, not {}",
            link.link.display(),
            actual.display(),
            link.target.display()
        );
    }
    Ok(())
}

fn activation_dir(package: &str) -> PathBuf {
    workspace::state_home()
        .join("forkpkg")
        .join("activations")
        .join(package)
}

fn user_bin_dir() -> Result<PathBuf> {
    Ok(home_dir()?.join(".local").join("bin"))
}

fn home_dir() -> Result<PathBuf> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or_else(|| anyhow!("HOME is not set to an absolute path"))
}

fn link_exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("failed to inspect {}", path.display())),
    }
}

fn remove_dir_if_empty(path: &Path) -> Result<()> {
    match fs::remove_dir(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::DirectoryNotEmpty => Ok(()),
        Err(error) => Err(error).with_context(|| format!("failed to remove {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use crate::metadata::{BaseMetadata, BuildMetadata, Metadata, PackageMetadata};

    use super::package_key;

    #[test]
    fn package_key_prefers_pname() {
        let metadata = Metadata {
            format: 1,
            package: PackageMetadata {
                installable: "nixpkgs#hello".to_owned(),
                flake_ref: "nixpkgs".to_owned(),
                attribute: "hello".to_owned(),
                system: "x86_64-linux".to_owned(),
                name: Some("hello-2.12.2".to_owned()),
                pname: Some("hello".to_owned()),
                version: Some("2.12.2".to_owned()),
            },
            base: BaseMetadata {
                nixpkgs_revision: None,
                nixpkgs_last_modified: None,
                nixpkgs_locked_nar_hash: None,
                nixpkgs_resolved_url: None,
                nixpkgs_path: None,
                derivation: "drv".to_owned(),
                output: "out".to_owned(),
                source: None,
                source_revision: None,
                source_hash: None,
                source_ca: None,
                post_patch_source: "src".to_owned(),
                post_patch_source_hash: None,
                git_commit: "commit".to_owned(),
            },
            build: BuildMetadata {
                strategy: "strategy".to_owned(),
                patch_handling: "patch".to_owned(),
            },
        };

        assert_eq!(package_key(&metadata), "hello");
    }
}
