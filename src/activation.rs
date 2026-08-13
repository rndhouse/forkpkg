use std::env;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::metadata::Metadata;
use crate::workspace::{self, Workspace};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActivationRecord {
    pub format: u32,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub key: String,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_blake3: Option<String>,
    pub previous: PreviousLink,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PreviousLink {
    Absent,
    BackedUp { backup: PathBuf },
}

#[derive(Debug, Clone)]
pub struct ActivationCheck {
    pub record: ActivationRecord,
    pub problems: Vec<String>,
}

impl ActivationCheck {
    pub fn is_ok(&self) -> bool {
        self.problems.is_empty()
    }
}

#[derive(Debug, Clone)]
pub enum ActivationRecordEntry {
    Valid {
        path: PathBuf,
        record: ActivationRecord,
    },
    Broken {
        path: PathBuf,
        problem: String,
    },
}

impl ActivationRecordEntry {
    pub fn path(&self) -> &Path {
        match self {
            ActivationRecordEntry::Valid { path, .. }
            | ActivationRecordEntry::Broken { path, .. } => path,
        }
    }
}

#[derive(Debug, Clone)]
struct ActivationPaths {
    activations_dir: PathBuf,
    user_bin_dir: PathBuf,
}

impl ActivationPaths {
    fn from_env() -> Result<Self> {
        Ok(Self {
            activations_dir: activations_dir()?,
            user_bin_dir: user_bin_dir()?,
        })
    }

    fn activation_dir(&self, key: &str) -> PathBuf {
        self.activations_dir.join(key)
    }

    fn record_path(&self, record: &ActivationRecord) -> PathBuf {
        self.activation_dir(&record.key).join("activation.toml")
    }
}

pub fn enable_path_shim(
    metadata: &Metadata,
    workspace: &Workspace,
    build_output: &Path,
) -> Result<ActivationRecord> {
    let paths = ActivationPaths::from_env()?;
    let record = plan_path_shim_with_paths(metadata, workspace, build_output, &paths)?;
    apply_enable_record(&record, &paths)?;
    Ok(record)
}

pub fn plan_path_shim(
    metadata: &Metadata,
    workspace: &Workspace,
    build_output: &Path,
) -> Result<ActivationRecord> {
    let paths = ActivationPaths::from_env()?;
    plan_path_shim_with_paths(metadata, workspace, build_output, &paths)
}

fn plan_path_shim_with_paths(
    metadata: &Metadata,
    workspace: &Workspace,
    build_output: &Path,
    paths: &ActivationPaths,
) -> Result<ActivationRecord> {
    let key = package_key(metadata);
    let package = package_display_name(metadata);
    let activation_dir = paths.activation_dir(&key);
    for record_path in record_paths_for_metadata(metadata, paths) {
        if record_path.exists() {
            bail!("{package} is already active; run forkpkg disable first");
        }
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

    let mut links = Vec::new();
    for target in entries {
        let name = target
            .file_name()
            .ok_or_else(|| anyhow!("output entry has no file name: {}", target.display()))?
            .to_string_lossy()
            .into_owned();
        let link = paths.user_bin_dir.join(&name);
        let backup = activation_dir.join("backups").join(&name);

        let previous = if link_exists(&link)? {
            if is_plain_directory(&link)? {
                bail!(
                    "refusing to replace existing directory in PATH: {}",
                    link.display()
                );
            }
            if link_exists(&backup)? {
                bail!(
                    "refusing to overwrite existing backup: {}",
                    backup.display()
                );
            }
            PreviousLink::BackedUp { backup }
        } else {
            PreviousLink::Absent
        };

        links.push(LinkRecord {
            name,
            link,
            target_blake3: Some(blake3_file(&target)?),
            target,
            previous,
        });
    }

    Ok(ActivationRecord {
        format: 1,
        key,
        mode: "path-shim".to_owned(),
        package,
        installable: metadata.package.installable.clone(),
        workspace: workspace.root.clone(),
        source: workspace.source.clone(),
        build_output: build_output.to_path_buf(),
        activated_at_unix: unix_time_secs()?,
        links,
    })
}

pub fn disable(metadata: &Metadata) -> Result<ActivationRecord> {
    let paths = ActivationPaths::from_env()?;
    let record = disable_plan_with_paths(metadata, &paths)?;
    apply_disable_record(&record, &paths)?;
    Ok(record)
}

pub fn disable_record(record: &ActivationRecord) -> Result<()> {
    let paths = ActivationPaths::from_env()?;
    apply_disable_record(record, &paths)
}

pub fn disable_plan(metadata: &Metadata) -> Result<ActivationRecord> {
    let paths = ActivationPaths::from_env()?;
    disable_plan_with_paths(metadata, &paths)
}

fn disable_plan_with_paths(
    metadata: &Metadata,
    paths: &ActivationPaths,
) -> Result<ActivationRecord> {
    let record = read_record_for_metadata(metadata, paths)?
        .ok_or_else(|| anyhow!("{} is not active", package_display_name(metadata)))?;
    preflight_disable_record(&record, paths)?;
    Ok(record)
}

pub fn list_record_entries() -> Result<Vec<ActivationRecordEntry>> {
    let paths = ActivationPaths::from_env()?;
    list_record_entries_with_paths(&paths)
}

fn list_record_entries_with_paths(paths: &ActivationPaths) -> Result<Vec<ActivationRecordEntry>> {
    if !paths.activations_dir.exists() {
        return Ok(Vec::new());
    }

    let mut entries = Vec::new();
    for entry in fs::read_dir(&paths.activations_dir)
        .with_context(|| format!("failed to read {}", paths.activations_dir.display()))?
    {
        let entry = entry.with_context(|| {
            format!(
                "failed to read entry in {}",
                paths.activations_dir.display()
            )
        })?;
        let record_path = entry.path().join("activation.toml");
        if !record_path.exists() {
            continue;
        }

        match read_record(&record_path) {
            Ok(record) => entries.push(ActivationRecordEntry::Valid {
                path: record_path,
                record,
            }),
            Err(error) => entries.push(ActivationRecordEntry::Broken {
                path: record_path,
                problem: format!("{error:#}"),
            }),
        }
    }

    entries.sort_by(|left, right| left.path().cmp(right.path()));
    Ok(entries)
}

pub fn check_record(record: ActivationRecord) -> ActivationCheck {
    let mut problems = Vec::new();

    if record.key.is_empty() {
        problems.push("activation record key is missing".to_owned());
    }
    if !record.workspace.join("forkpkg.toml").is_file() {
        problems.push(format!(
            "workspace metadata is missing: {}",
            record.workspace.join("forkpkg.toml").display()
        ));
    }
    if !record.source.is_dir() {
        problems.push(format!(
            "source directory is missing: {}",
            record.source.display()
        ));
    }
    if !record.build_output.exists() {
        problems.push(format!(
            "build output is missing: {}",
            record.build_output.display()
        ));
    }

    for link in &record.links {
        match fs::read_link(&link.link) {
            Ok(actual) => {
                if actual != link.target {
                    problems.push(format!(
                        "{} points to {}, expected {}",
                        link.link.display(),
                        actual.display(),
                        link.target.display()
                    ));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                problems.push(format!("active link is missing: {}", link.link.display()));
            }
            Err(error) => {
                problems.push(format!(
                    "active link is not readable as forkpkg symlink: {} ({error})",
                    link.link.display()
                ));
            }
        }

        if !link.target.exists() {
            problems.push(format!("target is missing: {}", link.target.display()));
        } else if let Some(expected) = &link.target_blake3 {
            match blake3_file(&link.target) {
                Ok(actual) if actual == *expected => {}
                Ok(actual) => problems.push(format!(
                    "target hash mismatch for {}: expected {}, got {}",
                    link.target.display(),
                    expected,
                    actual
                )),
                Err(error) => problems.push(format!(
                    "failed to hash target {}: {error}",
                    link.target.display()
                )),
            }
        }

        if let PreviousLink::BackedUp { backup } = &link.previous
            && !backup.exists()
        {
            problems.push(format!("backup is missing: {}", backup.display()));
        }
    }

    ActivationCheck { record, problems }
}

fn apply_enable_record(record: &ActivationRecord, paths: &ActivationPaths) -> Result<()> {
    preflight_enable_record(record, paths)?;

    let mut undo = Vec::new();
    let result = try_apply_enable_record(record, paths, &mut undo);
    if let Err(error) = result {
        if let Err(rollback_error) = rollback_enable(undo) {
            bail!("{error:#}; rollback after failed activation also failed: {rollback_error:#}");
        }
        return Err(error);
    }

    Ok(())
}

fn try_apply_enable_record(
    record: &ActivationRecord,
    paths: &ActivationPaths,
    undo: &mut Vec<EnableUndo>,
) -> Result<()> {
    let activation_dir = paths.activation_dir(&record.key);
    fs::create_dir_all(activation_dir.join("backups"))
        .with_context(|| format!("failed to create {}", activation_dir.display()))?;

    for link in &record.links {
        let parent = link
            .link
            .parent()
            .ok_or_else(|| anyhow!("link has no parent: {}", link.link.display()))?;
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;

        match &link.previous {
            PreviousLink::Absent => {
                if link_exists(&link.link)? {
                    bail!("refusing to replace newly-created {}", link.link.display());
                }
                symlink(&link.target, &link.link).with_context(|| {
                    format!(
                        "failed to link {} to {}",
                        link.link.display(),
                        link.target.display()
                    )
                })?;
                undo.push(EnableUndo::RemoveCreated {
                    link: link.link.clone(),
                });
            }
            PreviousLink::BackedUp { backup } => {
                fs::rename(&link.link, backup).with_context(|| {
                    format!(
                        "failed to move existing {} to {}",
                        link.link.display(),
                        backup.display()
                    )
                })?;
                undo.push(EnableUndo::RestorePrevious {
                    link: link.link.clone(),
                    backup: backup.clone(),
                });

                symlink(&link.target, &link.link).with_context(|| {
                    format!(
                        "failed to link {} to {}",
                        link.link.display(),
                        link.target.display()
                    )
                })?;
            }
        }
    }

    write_record_atomic(record, paths)
}

fn preflight_enable_record(record: &ActivationRecord, paths: &ActivationPaths) -> Result<()> {
    let record_path = paths.record_path(record);
    if record_path.exists() {
        bail!(
            "activation record already exists: {}",
            record_path.display()
        );
    }

    for link in &record.links {
        ensure_target_is_unchanged(link)?;
        match &link.previous {
            PreviousLink::Absent => {
                if link_exists(&link.link)? {
                    bail!("refusing to replace newly-created {}", link.link.display());
                }
            }
            PreviousLink::BackedUp { backup } => {
                if !link_exists(&link.link)? {
                    bail!("existing PATH entry disappeared: {}", link.link.display());
                }
                if is_plain_directory(&link.link)? {
                    bail!(
                        "refusing to replace existing directory in PATH: {}",
                        link.link.display()
                    );
                }
                if link_exists(backup)? {
                    bail!(
                        "refusing to overwrite existing backup: {}",
                        backup.display()
                    );
                }
            }
        }
    }

    Ok(())
}

fn apply_disable_record(record: &ActivationRecord, paths: &ActivationPaths) -> Result<()> {
    preflight_disable_record(record, paths)?;

    let mut undo = Vec::new();
    let result = try_apply_disable_record(record, paths, &mut undo);
    if let Err(error) = result {
        if let Err(rollback_error) = rollback_disable(undo) {
            bail!("{error:#}; rollback after failed deactivation also failed: {rollback_error:#}");
        }
        return Err(error);
    }

    Ok(())
}

fn try_apply_disable_record(
    record: &ActivationRecord,
    paths: &ActivationPaths,
    undo: &mut Vec<DisableUndo>,
) -> Result<()> {
    for link in &record.links {
        fs::remove_file(&link.link)
            .with_context(|| format!("failed to remove {}", link.link.display()))?;
        undo.push(DisableUndo::RecreateActiveLink {
            link: link.link.clone(),
            target: link.target.clone(),
        });

        if let PreviousLink::BackedUp { backup } = &link.previous {
            fs::rename(backup, &link.link).with_context(|| {
                format!(
                    "failed to restore {} to {}",
                    backup.display(),
                    link.link.display()
                )
            })?;
            undo.pop();
            undo.push(DisableUndo::MoveRestoredBack {
                link: link.link.clone(),
                target: link.target.clone(),
                backup: backup.clone(),
            });
        }
    }

    let record_path = paths.record_path(record);
    fs::remove_file(&record_path)
        .with_context(|| format!("failed to remove {}", record_path.display()))?;
    let dir = paths.activation_dir(&record.key);
    let _ = remove_dir_if_empty(&dir.join("backups"));
    let _ = remove_dir_if_empty(&dir);
    Ok(())
}

fn preflight_disable_record(record: &ActivationRecord, paths: &ActivationPaths) -> Result<()> {
    let record_path = paths.record_path(record);
    if !record_path.is_file() {
        bail!("activation record is missing: {}", record_path.display());
    }

    for link in &record.links {
        ensure_link_points_to_target(link)?;
        if let PreviousLink::BackedUp { backup } = &link.previous
            && !link_exists(backup)?
        {
            bail!("cannot restore missing backup: {}", backup.display());
        }
    }

    Ok(())
}

pub fn status(metadata: &Metadata) -> Result<Option<ActivationRecord>> {
    let paths = ActivationPaths::from_env()?;
    read_record_for_metadata(metadata, &paths)
}

pub fn package_key(metadata: &Metadata) -> String {
    workspace::stable_name(&package_display_name(metadata), &package_identity(metadata))
}

pub fn package_display_name(metadata: &Metadata) -> String {
    metadata
        .package
        .pname
        .clone()
        .or_else(|| metadata.package.name.clone())
        .unwrap_or_else(|| metadata.package.attribute.clone())
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
        if !(file_type.is_file() || file_type.is_symlink()) {
            continue;
        }

        let metadata =
            fs::metadata(&path).with_context(|| format!("failed to inspect {}", path.display()))?;
        if metadata.file_type().is_file() && metadata.permissions().mode() & 0o111 != 0 {
            entries.push(path);
        }
    }
    entries.sort();
    Ok(entries)
}

fn write_record_atomic(record: &ActivationRecord, paths: &ActivationPaths) -> Result<()> {
    let dir = paths.activation_dir(&record.key);
    fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
    let path = dir.join("activation.toml");
    let temporary = dir.join(format!(
        ".activation.toml.tmp.{}.{}",
        std::process::id(),
        unix_time_secs()?
    ));
    let text = toml::to_string_pretty(record).context("failed to serialize activation record")?;

    let write_result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .with_context(|| format!("failed to create {}", temporary.display()))?;
        file.write_all(text.as_bytes())
            .with_context(|| format!("failed to write {}", temporary.display()))?;
        file.sync_all()
            .with_context(|| format!("failed to sync {}", temporary.display()))?;
        fs::rename(&temporary, &path).with_context(|| {
            format!(
                "failed to move {} to {}",
                temporary.display(),
                path.display()
            )
        })?;
        Ok(())
    })();

    if write_result.is_err() {
        let _ = fs::remove_file(&temporary);
    }

    write_result
}

fn read_record_for_metadata(
    metadata: &Metadata,
    paths: &ActivationPaths,
) -> Result<Option<ActivationRecord>> {
    for record_path in record_paths_for_metadata(metadata, paths) {
        if record_path.is_file() {
            return read_record(&record_path).map(Some);
        }
    }

    Ok(None)
}

fn read_record(path: &Path) -> Result<ActivationRecord> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("failed to read activation record {}", path.display()))?;
    let mut record: ActivationRecord =
        toml::from_str(&text).with_context(|| format!("failed to parse {}", path.display()))?;
    if record.key.is_empty() {
        record.key = record.package.clone();
    }
    Ok(record)
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

    ensure_target_is_unchanged(link)
}

fn ensure_target_is_unchanged(link: &LinkRecord) -> Result<()> {
    if let Some(expected) = &link.target_blake3 {
        let actual_hash = blake3_file(&link.target)?;
        if actual_hash != *expected {
            bail!(
                "refusing to use {} because its hash changed: expected {}, got {}",
                link.target.display(),
                expected,
                actual_hash
            );
        }
    }

    Ok(())
}

fn blake3_file(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path)
        .with_context(|| format!("failed to open {} for hashing", path.display()))?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = [0u8; 64 * 1024];

    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("failed to read {} for hashing", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }

    Ok(hasher.finalize().to_hex().to_string())
}

pub fn activations_dir() -> Result<PathBuf> {
    Ok(workspace::state_home()?.join("forkpkg").join("activations"))
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

fn package_identity(metadata: &Metadata) -> String {
    format!(
        "\
installable={}\n\
flake_ref={}\n\
attribute={}\n\
system={}\n\
nixpkgs_revision={}\n\
nixpkgs_locked_nar_hash={}\n\
nixpkgs_resolved_url={}\n\
nixpkgs_path={}\n",
        metadata.package.installable,
        metadata.package.flake_ref,
        metadata.package.attribute,
        metadata.package.system,
        metadata.base.nixpkgs_revision.as_deref().unwrap_or(""),
        metadata
            .base
            .nixpkgs_locked_nar_hash
            .as_deref()
            .unwrap_or(""),
        metadata.base.nixpkgs_resolved_url.as_deref().unwrap_or(""),
        metadata.base.nixpkgs_path.as_deref().unwrap_or(""),
    )
}

fn record_paths_for_metadata(metadata: &Metadata, paths: &ActivationPaths) -> Vec<PathBuf> {
    let current_key = package_key(metadata);
    let legacy_key = workspace::sanitize_workspace_name(&package_display_name(metadata));
    let mut keys = vec![current_key];
    if keys[0] != legacy_key {
        keys.push(legacy_key);
    }

    keys.into_iter()
        .map(|key| paths.activation_dir(&key).join("activation.toml"))
        .collect()
}

fn link_exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("failed to inspect {}", path.display())),
    }
}

fn is_plain_directory(path: &Path) -> Result<bool> {
    Ok(fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect {}", path.display()))?
        .file_type()
        .is_dir())
}

fn remove_dir_if_empty(path: &Path) -> Result<()> {
    match fs::remove_dir(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::DirectoryNotEmpty => Ok(()),
        Err(error) => Err(error).with_context(|| format!("failed to remove {}", path.display())),
    }
}

fn unix_time_secs() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system time is before Unix epoch")?
        .as_secs())
}

#[derive(Debug)]
enum EnableUndo {
    RemoveCreated { link: PathBuf },
    RestorePrevious { link: PathBuf, backup: PathBuf },
}

fn rollback_enable(mut undo: Vec<EnableUndo>) -> Result<()> {
    let mut errors = Vec::new();
    while let Some(action) = undo.pop() {
        let result: Result<()> = match action {
            EnableUndo::RemoveCreated { link } => {
                if link_exists(&link)? {
                    fs::remove_file(&link)
                        .with_context(|| format!("failed to remove {}", link.display()))
                } else {
                    Ok(())
                }
            }
            EnableUndo::RestorePrevious { link, backup } => {
                if link_exists(&link)? {
                    fs::remove_file(&link)
                        .with_context(|| format!("failed to remove {}", link.display()))?;
                }
                if link_exists(&backup)? {
                    fs::rename(&backup, &link).with_context(|| {
                        format!(
                            "failed to restore {} to {}",
                            backup.display(),
                            link.display()
                        )
                    })
                } else {
                    Ok(())
                }
            }
        };

        if let Err(error) = result {
            errors.push(format!("{error:#}"));
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        bail!("{}", errors.join("; "))
    }
}

#[derive(Debug)]
enum DisableUndo {
    RecreateActiveLink {
        link: PathBuf,
        target: PathBuf,
    },
    MoveRestoredBack {
        link: PathBuf,
        target: PathBuf,
        backup: PathBuf,
    },
}

fn rollback_disable(mut undo: Vec<DisableUndo>) -> Result<()> {
    let mut errors = Vec::new();
    while let Some(action) = undo.pop() {
        let result: Result<()> = match action {
            DisableUndo::RecreateActiveLink { link, target } => {
                if !link_exists(&link)? {
                    symlink(&target, &link).with_context(|| {
                        format!("failed to link {} to {}", link.display(), target.display())
                    })?;
                }
                Ok(())
            }
            DisableUndo::MoveRestoredBack {
                link,
                target,
                backup,
            } => {
                if link_exists(&link)? {
                    fs::rename(&link, &backup).with_context(|| {
                        format!(
                            "failed to move restored {} back to {}",
                            link.display(),
                            backup.display()
                        )
                    })?;
                }
                if !link_exists(&link)? {
                    symlink(&target, &link).with_context(|| {
                        format!("failed to link {} to {}", link.display(), target.display())
                    })?;
                }
                Ok(())
            }
        };

        if let Err(error) = result {
            errors.push(format!("{error:#}"));
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        bail!("{}", errors.join("; "))
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    use tempfile::TempDir;

    use crate::metadata::{BaseMetadata, BuildMetadata, Metadata, PackageMetadata};

    use super::{
        ActivationPaths, ActivationRecordEntry, PreviousLink, apply_disable_record,
        apply_enable_record, executable_entries, list_record_entries_with_paths, package_key,
        plan_path_shim_with_paths, read_record, read_record_for_metadata,
    };

    #[test]
    fn package_key_prefers_pname_and_includes_identity_hash() {
        let metadata = metadata_for("nixpkgs#hello", "hello", "x86_64-linux");

        assert!(package_key(&metadata).starts_with("hello-"));
    }

    #[test]
    fn package_key_distinguishes_systems() {
        let first = metadata_for("nixpkgs#hello", "hello", "x86_64-linux");
        let second = metadata_for("nixpkgs#hello", "hello", "aarch64-linux");

        assert_ne!(package_key(&first), package_key(&second));
    }

    #[test]
    fn executable_entries_skip_non_executable_files() {
        let temp = TempDir::new().unwrap();
        let bin = temp.path();
        write_mode(&bin.join("tool"), "#!/bin/sh\n", 0o755);
        write_mode(&bin.join("data"), "payload\n", 0o644);
        fs::create_dir(bin.join("nested")).unwrap();

        let entries = executable_entries(bin).unwrap();

        assert_eq!(entries, [bin.join("tool")]);
    }

    #[test]
    fn enable_preflight_refuses_raced_path_without_mutation() {
        let fixture = Fixture::new();
        let metadata = metadata_for("nixpkgs#hello", "hello", "x86_64-linux");
        fixture.write_executable("a");
        fixture.write_executable("b");
        fs::write(fixture.paths.user_bin_dir.join("a"), "original\n").unwrap();

        let record = plan_path_shim_with_paths(
            &metadata,
            &fixture.workspace,
            &fixture.build_output,
            &fixture.paths,
        )
        .unwrap();
        fs::write(fixture.paths.user_bin_dir.join("b"), "raced\n").unwrap();

        let error = apply_enable_record(&record, &fixture.paths).unwrap_err();

        assert!(error.to_string().contains("newly-created"));
        assert_eq!(
            fs::read_to_string(fixture.paths.user_bin_dir.join("a")).unwrap(),
            "original\n"
        );
        assert!(!fixture.record_path(&record).exists());
    }

    #[test]
    fn disable_refuses_missing_backup_without_removing_active_link() {
        let fixture = Fixture::new();
        let metadata = metadata_for("nixpkgs#hello", "hello", "x86_64-linux");
        fixture.write_executable("hello");
        let mut record = plan_path_shim_with_paths(
            &metadata,
            &fixture.workspace,
            &fixture.build_output,
            &fixture.paths,
        )
        .unwrap();
        let backup = fixture
            .paths
            .activation_dir(&record.key)
            .join("backups/hello");
        record.links[0].previous = PreviousLink::BackedUp { backup };
        fs::create_dir_all(fixture.paths.activation_dir(&record.key)).unwrap();
        super::write_record_atomic(&record, &fixture.paths).unwrap();
        std::os::unix::fs::symlink(&record.links[0].target, &record.links[0].link).unwrap();

        let error = apply_disable_record(&record, &fixture.paths).unwrap_err();

        assert!(error.to_string().contains("missing backup"));
        assert_eq!(
            fs::read_link(&record.links[0].link).unwrap(),
            record.links[0].target
        );
    }

    #[test]
    fn disable_rolls_back_when_record_removal_fails() {
        let fixture = Fixture::new();
        let metadata = metadata_for("nixpkgs#hello", "hello", "x86_64-linux");
        fixture.write_executable("hello");
        fs::write(fixture.paths.user_bin_dir.join("hello"), "original\n").unwrap();
        let record = plan_path_shim_with_paths(
            &metadata,
            &fixture.workspace,
            &fixture.build_output,
            &fixture.paths,
        )
        .unwrap();
        apply_enable_record(&record, &fixture.paths).unwrap();
        let activation_dir = fixture.paths.activation_dir(&record.key);
        fs::set_permissions(&activation_dir, fs::Permissions::from_mode(0o555)).unwrap();

        let error = apply_disable_record(&record, &fixture.paths).unwrap_err();

        fs::set_permissions(&activation_dir, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(error.to_string().contains("failed to remove"));
        assert_eq!(
            fs::read_link(&record.links[0].link).unwrap(),
            record.links[0].target
        );
        let backup = match &record.links[0].previous {
            PreviousLink::BackedUp { backup } => backup,
            PreviousLink::Absent => panic!("expected backup"),
        };
        assert_eq!(fs::read_to_string(backup).unwrap(), "original\n");
    }

    #[test]
    fn list_record_entries_reports_broken_records() {
        let fixture = Fixture::new();
        let dir = fixture.paths.activations_dir.join("broken");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("activation.toml"), "not = [valid").unwrap();

        let entries = list_record_entries_with_paths(&fixture.paths).unwrap();

        assert_eq!(entries.len(), 1);
        assert!(matches!(
            &entries[0],
            ActivationRecordEntry::Broken { problem, .. } if problem.contains("failed to parse")
        ));
    }

    #[test]
    fn metadata_lookup_falls_back_to_legacy_activation_key() {
        let fixture = Fixture::new();
        let metadata = metadata_for("nixpkgs#hello", "hello", "x86_64-linux");
        fixture.write_executable("hello");
        let mut record = plan_path_shim_with_paths(
            &metadata,
            &fixture.workspace,
            &fixture.build_output,
            &fixture.paths,
        )
        .unwrap();
        record.key = "hello".to_owned();
        super::write_record_atomic(&record, &fixture.paths).unwrap();

        let found = read_record_for_metadata(&metadata, &fixture.paths)
            .unwrap()
            .unwrap();

        assert_eq!(found.key, "hello");
        assert!(
            plan_path_shim_with_paths(
                &metadata,
                &fixture.workspace,
                &fixture.build_output,
                &fixture.paths,
            )
            .unwrap_err()
            .to_string()
            .contains("already active")
        );
    }

    #[test]
    fn read_record_defaults_legacy_key_to_package() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("activation.toml");
        fs::write(
            &path,
            r#"
format = 1
mode = "path-shim"
package = "hello"
installable = "nixpkgs#hello"
workspace = "/tmp/workspace"
source = "/tmp/workspace/source"
build_output = "/tmp/output"
activated_at_unix = 1
links = []
"#,
        )
        .unwrap();

        let record = read_record(&path).unwrap();

        assert_eq!(record.key, "hello");
    }

    struct Fixture {
        _temp: TempDir,
        paths: ActivationPaths,
        workspace: crate::workspace::Workspace,
        build_output: std::path::PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let temp = TempDir::new().unwrap();
            let root = temp.path().to_path_buf();
            let workspace_root = root.join("workspace");
            let source = workspace_root.join("source");
            let build_output = root.join("build-output");
            let user_bin = root.join("home/.local/bin");
            let activations = root.join("state/forkpkg/activations");
            fs::create_dir_all(&source).unwrap();
            fs::create_dir_all(build_output.join("bin")).unwrap();
            fs::create_dir_all(&user_bin).unwrap();
            fs::create_dir_all(&activations).unwrap();
            fs::write(workspace_root.join("forkpkg.toml"), "").unwrap();

            Self {
                _temp: temp,
                paths: ActivationPaths {
                    activations_dir: activations,
                    user_bin_dir: user_bin,
                },
                workspace: crate::workspace::Workspace::new(workspace_root),
                build_output,
            }
        }

        fn write_executable(&self, name: &str) {
            write_mode(
                &self.build_output.join("bin").join(name),
                "#!/bin/sh\n",
                0o755,
            );
        }

        fn record_path(&self, record: &super::ActivationRecord) -> std::path::PathBuf {
            self.paths
                .activation_dir(&record.key)
                .join("activation.toml")
        }
    }

    fn metadata_for(installable: &str, attribute: &str, system: &str) -> Metadata {
        Metadata {
            format: 1,
            package: PackageMetadata {
                installable: installable.to_owned(),
                flake_ref: installable.split_once('#').unwrap().0.to_owned(),
                attribute: attribute.to_owned(),
                system: system.to_owned(),
                name: Some(format!("{attribute}-2.12.2")),
                pname: Some(attribute.to_owned()),
                version: Some("2.12.2".to_owned()),
            },
            base: BaseMetadata {
                nixpkgs_revision: Some("abc123".to_owned()),
                nixpkgs_last_modified: None,
                nixpkgs_locked_nar_hash: Some("sha256-test".to_owned()),
                nixpkgs_resolved_url: Some("github:NixOS/nixpkgs/abc123".to_owned()),
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
        }
    }

    fn write_mode(path: &Path, text: &str, mode: u32) {
        fs::write(path, text).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }
}
