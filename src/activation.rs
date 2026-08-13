use std::collections::BTreeMap;
use std::env;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::metadata::Metadata;
use crate::nix;
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<LinkRecord>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub services: Vec<ServiceRecord>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub profiles: Vec<ProfileRecord>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub modules: Vec<ModuleRecord>,
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
pub struct ServiceRecord {
    pub service: String,
    pub service_file: PathBuf,
    pub override_path: PathBuf,
    pub exec_start: String,
    pub target: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_blake3: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfileRecord {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<PathBuf>,
    pub store_path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub element: Option<String>,
    pub priority: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModuleRecord {
    pub backend: String,
    pub overlay: PathBuf,
    pub module: PathBuf,
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
        record: Box<ActivationRecord>,
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
    #[allow(dead_code)]
    user_bin_dir: PathBuf,
    #[allow(dead_code)]
    user_config_dir: PathBuf,
}

impl ActivationPaths {
    fn from_env() -> Result<Self> {
        Ok(Self {
            activations_dir: activations_dir()?,
            user_bin_dir: user_bin_dir()?,
            user_config_dir: user_config_dir()?,
        })
    }

    fn activation_dir(&self, key: &str) -> PathBuf {
        self.activations_dir.join(key)
    }

    fn record_path(&self, record: &ActivationRecord) -> PathBuf {
        self.activation_dir(&record.key).join("activation.toml")
    }
}

#[allow(dead_code)]
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

#[allow(dead_code)]
pub fn plan_path_shim(
    metadata: &Metadata,
    workspace: &Workspace,
    build_output: &Path,
) -> Result<ActivationRecord> {
    let paths = ActivationPaths::from_env()?;
    plan_path_shim_with_paths(metadata, workspace, build_output, &paths)
}

pub fn enable_nix_profile(
    metadata: &Metadata,
    workspace: &Workspace,
    build_output: &Path,
    profile: Option<&Path>,
) -> Result<ActivationRecord> {
    let paths = ActivationPaths::from_env()?;
    let mut record =
        plan_nix_profile_with_paths(metadata, workspace, build_output, profile, &paths)?;
    let profile_record = record
        .profiles
        .first()
        .ok_or_else(|| anyhow!("nix-profile activation record has no profile entry"))?;

    nix_profile_add(
        profile_record.profile.as_deref(),
        &profile_record.store_path,
        profile_record.priority,
    )?;

    if let Some(profile_record) = record.profiles.first_mut() {
        profile_record.element = find_profile_element(
            profile_record.profile.as_deref(),
            &profile_record.store_path,
        )?;
    }

    write_record_atomic(&record, &paths)?;
    Ok(record)
}

pub fn plan_nix_profile(
    metadata: &Metadata,
    workspace: &Workspace,
    build_output: &Path,
    profile: Option<&Path>,
) -> Result<ActivationRecord> {
    let paths = ActivationPaths::from_env()?;
    plan_nix_profile_with_paths(metadata, workspace, build_output, profile, &paths)
}

pub fn enable_nixos_module(
    metadata: &Metadata,
    workspace: &Workspace,
    build_output: &Path,
) -> Result<ActivationRecord> {
    enable_nix_module(metadata, workspace, build_output, "nixos-module")
}

pub fn plan_nixos_module(
    metadata: &Metadata,
    workspace: &Workspace,
    build_output: &Path,
) -> Result<ActivationRecord> {
    let paths = ActivationPaths::from_env()?;
    plan_nix_module_with_paths(metadata, workspace, build_output, "nixos-module", &paths)
}

pub fn enable_home_manager_module(
    metadata: &Metadata,
    workspace: &Workspace,
    build_output: &Path,
) -> Result<ActivationRecord> {
    enable_nix_module(metadata, workspace, build_output, "home-manager-module")
}

pub fn plan_home_manager_module(
    metadata: &Metadata,
    workspace: &Workspace,
    build_output: &Path,
) -> Result<ActivationRecord> {
    let paths = ActivationPaths::from_env()?;
    plan_nix_module_with_paths(
        metadata,
        workspace,
        build_output,
        "home-manager-module",
        &paths,
    )
}

fn enable_nix_module(
    metadata: &Metadata,
    workspace: &Workspace,
    build_output: &Path,
    backend: &str,
) -> Result<ActivationRecord> {
    let paths = ActivationPaths::from_env()?;
    let record = plan_nix_module_with_paths(metadata, workspace, build_output, backend, &paths)?;
    let module = record
        .modules
        .first()
        .ok_or_else(|| anyhow!("module activation record has no module entry"))?;

    write_enabled_module_files(module, metadata, workspace)?;
    write_record_atomic(&record, &paths)?;
    Ok(record)
}

#[allow(dead_code)]
pub fn enable_systemd_user_service(
    metadata: &Metadata,
    workspace: &Workspace,
    build_output: &Path,
    service: &str,
    service_file: &Path,
    exec_start: &str,
    target: &Path,
) -> Result<ActivationRecord> {
    let paths = ActivationPaths::from_env()?;
    let record = plan_systemd_user_service_with_paths(
        metadata,
        workspace,
        build_output,
        service,
        service_file,
        exec_start,
        target,
        &paths,
    )?;
    apply_enable_record(&record, &paths)?;
    Ok(record)
}

#[allow(dead_code)]
pub fn plan_systemd_user_service(
    metadata: &Metadata,
    workspace: &Workspace,
    build_output: &Path,
    service: &str,
    service_file: &Path,
    exec_start: &str,
    target: &Path,
) -> Result<ActivationRecord> {
    let paths = ActivationPaths::from_env()?;
    plan_systemd_user_service_with_paths(
        metadata,
        workspace,
        build_output,
        service,
        service_file,
        exec_start,
        target,
        &paths,
    )
}

#[allow(dead_code)]
#[allow(clippy::too_many_arguments)]
fn plan_systemd_user_service_with_paths(
    metadata: &Metadata,
    workspace: &Workspace,
    build_output: &Path,
    service: &str,
    service_file: &Path,
    exec_start: &str,
    target: &Path,
    paths: &ActivationPaths,
) -> Result<ActivationRecord> {
    let key = fork_key(metadata);
    let package = fork_display_name(metadata);
    for record_path in record_paths_for_metadata(metadata, paths) {
        if record_path.exists() {
            bail!("{package} is already active; run forkpkg disable first");
        }
    }

    if !target.is_absolute() {
        bail!(
            "systemd user service target is not absolute: {}",
            target.display()
        );
    }
    if !target.exists() {
        bail!(
            "systemd user service target does not exist: {}",
            target.display()
        );
    }

    let override_path = paths
        .user_config_dir
        .join("systemd")
        .join("user")
        .join(format!("{service}.d"))
        .join("forkpkg.conf");
    if link_exists(&override_path)? {
        bail!(
            "refusing to overwrite existing systemd override: {}",
            override_path.display()
        );
    }

    Ok(ActivationRecord {
        format: 1,
        key,
        mode: "systemd-user-service".to_owned(),
        package,
        installable: metadata.package.installable.clone(),
        workspace: workspace.root.clone(),
        source: workspace.source.clone(),
        build_output: build_output.to_path_buf(),
        activated_at_unix: unix_time_secs()?,
        links: Vec::new(),
        services: vec![ServiceRecord {
            service: service.to_owned(),
            service_file: service_file.to_path_buf(),
            override_path,
            exec_start: exec_start.to_owned(),
            target: target.to_path_buf(),
            target_blake3: Some(blake3_file(target)?),
        }],
        profiles: Vec::new(),
        modules: Vec::new(),
    })
}

fn plan_nix_profile_with_paths(
    metadata: &Metadata,
    workspace: &Workspace,
    build_output: &Path,
    profile: Option<&Path>,
    paths: &ActivationPaths,
) -> Result<ActivationRecord> {
    let key = fork_key(metadata);
    let package = fork_display_name(metadata);
    for record_path in record_paths_for_metadata(metadata, paths) {
        if record_path.exists() {
            bail!("{package} is already active; run forkpkg disable first");
        }
    }

    if !build_output.is_absolute() {
        bail!("build output is not absolute: {}", build_output.display());
    }
    if !build_output.exists() {
        bail!("build output does not exist: {}", build_output.display());
    }

    Ok(ActivationRecord {
        format: 1,
        key,
        mode: "nix-profile".to_owned(),
        package,
        installable: metadata.package.installable.clone(),
        workspace: workspace.root.clone(),
        source: workspace.source.clone(),
        build_output: build_output.to_path_buf(),
        activated_at_unix: unix_time_secs()?,
        links: Vec::new(),
        services: Vec::new(),
        profiles: vec![ProfileRecord {
            profile: profile.map(Path::to_path_buf),
            store_path: build_output.to_path_buf(),
            element: None,
            priority: 1,
        }],
        modules: Vec::new(),
    })
}

fn plan_nix_module_with_paths(
    metadata: &Metadata,
    workspace: &Workspace,
    build_output: &Path,
    backend: &str,
    paths: &ActivationPaths,
) -> Result<ActivationRecord> {
    let key = fork_key(metadata);
    let package = fork_display_name(metadata);
    for record_path in record_paths_for_metadata(metadata, paths) {
        if record_path.exists() {
            bail!("{package} is already active; run forkpkg disable first");
        }
    }

    if !build_output.is_absolute() {
        bail!("build output is not absolute: {}", build_output.display());
    }
    if !build_output.exists() {
        bail!("build output does not exist: {}", build_output.display());
    }

    let nix_dir = paths.activation_dir(&key).join("nix");
    let module_name = match backend {
        "nixos-module" => "nixos-module.nix",
        "home-manager-module" => "home-manager-module.nix",
        other => bail!("unsupported Nix module backend: {other}"),
    };

    Ok(ActivationRecord {
        format: 1,
        key,
        mode: backend.to_owned(),
        package,
        installable: metadata.package.installable.clone(),
        workspace: workspace.root.clone(),
        source: workspace.source.clone(),
        build_output: build_output.to_path_buf(),
        activated_at_unix: unix_time_secs()?,
        links: Vec::new(),
        services: Vec::new(),
        profiles: Vec::new(),
        modules: vec![ModuleRecord {
            backend: backend.to_owned(),
            overlay: nix_dir.join("overlay.nix"),
            module: nix_dir.join(module_name),
        }],
    })
}

#[allow(dead_code)]
fn plan_path_shim_with_paths(
    metadata: &Metadata,
    workspace: &Workspace,
    build_output: &Path,
    paths: &ActivationPaths,
) -> Result<ActivationRecord> {
    let key = fork_key(metadata);
    let package = fork_display_name(metadata);
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
    let active_records = list_record_entries_with_paths(paths)?;
    for target in entries {
        let name = target
            .file_name()
            .ok_or_else(|| anyhow!("output entry has no file name: {}", target.display()))?
            .to_string_lossy()
            .into_owned();
        let link = paths.user_bin_dir.join(&name);
        let backup = activation_dir.join("backups").join(&name);

        for entry in &active_records {
            let ActivationRecordEntry::Valid { record, .. } = entry else {
                continue;
            };
            if record.key == key {
                continue;
            }
            if record.links.iter().any(|existing| existing.link == link) {
                bail!(
                    "{} is already managed by active fork {}; run forkpkg disable {} first",
                    link.display(),
                    record.package,
                    record.package
                );
            }
        }

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
        services: Vec::new(),
        profiles: Vec::new(),
        modules: Vec::new(),
    })
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
        .ok_or_else(|| anyhow!("{} is not active", fork_display_name(metadata)))?;
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
    collect_record_entries(&paths.activations_dir, &mut entries)?;
    entries.sort_by(|left, right| left.path().cmp(right.path()));
    Ok(entries)
}

fn collect_record_entries(dir: &Path, entries: &mut Vec<ActivationRecordEntry>) -> Result<()> {
    for entry in fs::read_dir(dir).with_context(|| format!("failed to read {}", dir.display()))? {
        let entry = entry.with_context(|| format!("failed to read entry in {}", dir.display()))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .with_context(|| format!("failed to inspect {}", path.display()))?;
        if !file_type.is_dir() {
            continue;
        }

        let record_path = path.join("activation.toml");
        if record_path.exists() {
            match read_record(&record_path) {
                Ok(record) => entries.push(ActivationRecordEntry::Valid {
                    path: record_path,
                    record: Box::new(record),
                }),
                Err(error) => entries.push(ActivationRecordEntry::Broken {
                    path: record_path,
                    problem: format!("{error:#}"),
                }),
            }
            continue;
        }

        collect_record_entries(&path, entries)?;
    }

    Ok(())
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

    for service in &record.services {
        if !service.service_file.exists() {
            problems.push(format!(
                "service file is missing: {}",
                service.service_file.display()
            ));
        }
        if !service.override_path.is_file() {
            problems.push(format!(
                "systemd override is missing: {}",
                service.override_path.display()
            ));
        } else {
            match fs::read_to_string(&service.override_path) {
                Ok(text) if text.contains(&service.exec_start) => {}
                Ok(_) => problems.push(format!(
                    "systemd override no longer points to {}",
                    service.exec_start
                )),
                Err(error) => problems.push(format!(
                    "failed to read systemd override {}: {error}",
                    service.override_path.display()
                )),
            }
        }
        if !service.target.exists() {
            problems.push(format!("target is missing: {}", service.target.display()));
        } else if let Some(expected) = &service.target_blake3 {
            match blake3_file(&service.target) {
                Ok(actual) if actual == *expected => {}
                Ok(actual) => problems.push(format!(
                    "target hash mismatch for {}: expected {}, got {}",
                    service.target.display(),
                    expected,
                    actual
                )),
                Err(error) => problems.push(format!(
                    "failed to hash target {}: {error}",
                    service.target.display()
                )),
            }
        }
    }

    for profile in &record.profiles {
        match profile_contains_store_path(profile.profile.as_deref(), &profile.store_path) {
            Ok(true) => {}
            Ok(false) => problems.push(format!(
                "Nix profile does not contain {}",
                profile.store_path.display()
            )),
            Err(error) => problems.push(format!("failed to inspect Nix profile: {error:#}")),
        }
    }

    for module in &record.modules {
        if !module.overlay.is_file() {
            problems.push(format!(
                "Nix overlay is missing: {}",
                module.overlay.display()
            ));
        }
        if !module.module.is_file() {
            problems.push(format!(
                "Nix module is missing: {}",
                module.module.display()
            ));
        } else {
            match fs::read_to_string(&module.module) {
                Ok(text) if text.contains("forkpkg: enabled") => {}
                Ok(_) => problems.push(format!(
                    "Nix module is not marked enabled: {}",
                    module.module.display()
                )),
                Err(error) => problems.push(format!(
                    "failed to read Nix module {}: {error}",
                    module.module.display()
                )),
            }
        }
    }

    ActivationCheck { record, problems }
}

#[allow(dead_code)]
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

#[allow(dead_code)]
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

    for service in &record.services {
        write_service_override_atomic(service)?;
        undo.push(EnableUndo::RemoveCreatedServiceOverride {
            service: service.service.clone(),
            override_path: service.override_path.clone(),
        });
        systemctl_user(["daemon-reload"])?;
        systemctl_user(["restart", service.service.as_str()])?;
    }

    write_record_atomic(record, paths)
}

#[allow(dead_code)]
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

    for service in &record.services {
        ensure_target_path_is_unchanged(&service.target, service.target_blake3.as_ref())?;
        if link_exists(&service.override_path)? {
            bail!(
                "refusing to overwrite existing systemd override: {}",
                service.override_path.display()
            );
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

    for service in &record.services {
        let previous_text = fs::read_to_string(&service.override_path).with_context(|| {
            format!(
                "failed to read systemd override {}",
                service.override_path.display()
            )
        })?;
        fs::remove_file(&service.override_path).with_context(|| {
            format!(
                "failed to remove systemd override {}",
                service.override_path.display()
            )
        })?;
        undo.push(DisableUndo::RestoreServiceOverride {
            service: service.service.clone(),
            override_path: service.override_path.clone(),
            text: previous_text,
        });
        systemctl_user(["daemon-reload"])?;
        systemctl_user(["restart", service.service.as_str()])?;
    }

    for profile in &record.profiles {
        if profile_contains_store_path(profile.profile.as_deref(), &profile.store_path)? {
            nix_profile_remove(profile.profile.as_deref(), &profile.store_path)?;
            undo.push(DisableUndo::ReinstallProfile {
                profile: profile.profile.clone(),
                store_path: profile.store_path.clone(),
                priority: profile.priority,
            });
        }
    }

    for module in &record.modules {
        let previous_text = fs::read_to_string(&module.module)
            .with_context(|| format!("failed to read Nix module {}", module.module.display()))?;
        write_disabled_module_file(module)?;
        undo.push(DisableUndo::RestoreModule {
            module: module.module.clone(),
            text: previous_text,
        });
    }

    let record_path = paths.record_path(record);
    fs::remove_file(&record_path)
        .with_context(|| format!("failed to remove {}", record_path.display()))?;
    let dir = paths.activation_dir(&record.key);
    let _ = remove_dir_if_empty(&dir.join("backups"));
    let _ = remove_dir_if_empty(&dir);
    for service in &record.services {
        if let Some(parent) = service.override_path.parent() {
            let _ = remove_dir_if_empty(parent);
        }
    }
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

    for service in &record.services {
        ensure_target_path_is_unchanged(&service.target, service.target_blake3.as_ref())?;
        if !service.override_path.is_file() {
            bail!(
                "systemd override is missing: {}",
                service.override_path.display()
            );
        }
        let text = fs::read_to_string(&service.override_path).with_context(|| {
            format!(
                "failed to read systemd override {}",
                service.override_path.display()
            )
        })?;
        if !text.contains(&service.exec_start) {
            bail!(
                "refusing to disable because systemd override no longer points to {}",
                service.exec_start
            );
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

pub fn fork_key(metadata: &Metadata) -> String {
    format!(
        "{}/{}",
        package_key(metadata),
        workspace::sanitize_workspace_name(metadata.fork_label())
    )
}

pub fn fork_display_name(metadata: &Metadata) -> String {
    format!(
        "{}/{}",
        workspace::sanitize_workspace_name(&package_display_name(metadata)),
        workspace::sanitize_workspace_name(metadata.fork_label())
    )
}

pub fn package_display_name(metadata: &Metadata) -> String {
    metadata
        .package
        .pname
        .clone()
        .or_else(|| metadata.package.name.clone())
        .unwrap_or_else(|| metadata.package.attribute.clone())
}

#[allow(dead_code)]
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

#[allow(dead_code)]
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
    ensure_target_path_is_unchanged(&link.target, link.target_blake3.as_ref())
}

fn ensure_target_path_is_unchanged(path: &Path, expected_hash: Option<&String>) -> Result<()> {
    if !path.exists() {
        bail!("target is missing: {}", path.display());
    }

    if let Some(expected) = expected_hash {
        let actual_hash = blake3_file(path)?;
        if actual_hash != *expected {
            bail!(
                "refusing to use {} because its hash changed: expected {}, got {}",
                path.display(),
                expected,
                actual_hash
            );
        }
    }

    Ok(())
}

fn write_enabled_module_files(
    module: &ModuleRecord,
    metadata: &Metadata,
    workspace: &Workspace,
) -> Result<()> {
    let overlay = nix::local_source_overlay_expr(metadata, &workspace.source)?;
    write_text_atomic(&module.overlay, &overlay)?;
    write_text_atomic(&module.module, &enabled_module_text(module))?;
    Ok(())
}

fn write_disabled_module_file(module: &ModuleRecord) -> Result<()> {
    write_text_atomic(&module.module, &disabled_module_text(module))
}

fn enabled_module_text(module: &ModuleRecord) -> String {
    format!(
        "\
# forkpkg: enabled
{{ ... }}:
{{
  nixpkgs.overlays = [ (import {}) ];
}}
",
        nix_path_literal(&module.overlay)
    )
}

fn disabled_module_text(module: &ModuleRecord) -> String {
    format!(
        "\
# forkpkg: disabled
{{ ... }}:
{{
}}
# Previous backend: {}
",
        module.backend
    )
}

fn write_text_atomic(path: &Path, text: &str) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("path has no parent: {}", path.display()))?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;

    let temporary = parent.join(format!(
        ".{}.tmp.{}.{}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("forkpkg"),
        std::process::id(),
        unix_time_secs()?
    ));

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
        fs::rename(&temporary, path).with_context(|| {
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

fn nix_path_literal(path: &Path) -> String {
    let path = path.to_string_lossy();
    format!("(/. + {})", nix_string(&path))
}

fn nix_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(ch),
        }
    }
    out.push('"');
    out
}

#[derive(Debug, Deserialize)]
struct NixProfileList {
    elements: BTreeMap<String, NixProfileElement>,
}

#[derive(Debug, Deserialize)]
struct NixProfileElement {
    #[serde(default)]
    active: bool,
    #[serde(default, rename = "storePaths")]
    store_paths: Vec<PathBuf>,
}

pub fn profile_contains_store_path(profile: Option<&Path>, store_path: &Path) -> Result<bool> {
    Ok(find_profile_element(profile, store_path)?.is_some())
}

fn find_profile_element(profile: Option<&Path>, store_path: &Path) -> Result<Option<String>> {
    let list = nix_profile_list(profile)?;
    Ok(list.elements.into_iter().find_map(|(name, element)| {
        (element.active && element.store_paths.iter().any(|path| path == store_path))
            .then_some(name)
    }))
}

fn nix_profile_add(profile: Option<&Path>, store_path: &Path, priority: i64) -> Result<()> {
    let priority = priority.to_string();
    let mut command = nix_command();
    command.args(["profile", "add", "--priority", &priority]);
    if let Some(profile) = profile {
        command.arg("--profile").arg(profile);
    }
    command.arg(store_path);
    run_command(&mut command, "nix profile add")
}

fn nix_profile_remove(profile: Option<&Path>, store_path: &Path) -> Result<()> {
    let mut command = nix_command();
    command.args(["profile", "remove"]);
    if let Some(profile) = profile {
        command.arg("--profile").arg(profile);
    }
    command.arg(store_path);
    run_command(&mut command, "nix profile remove")
}

fn nix_profile_list(profile: Option<&Path>) -> Result<NixProfileList> {
    let mut command = nix_command();
    command.args(["profile", "list", "--json"]);
    if let Some(profile) = profile {
        command.arg("--profile").arg(profile);
    }

    let output = command
        .output()
        .context("failed to execute nix profile list")?;
    if !output.status.success() {
        return Err(anyhow!(
            "nix profile list failed with status {}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    serde_json::from_slice(&output.stdout).context("failed to parse nix profile JSON")
}

fn nix_command() -> Command {
    let mut command = Command::new("nix");
    command.args(["--extra-experimental-features", "nix-command flakes"]);
    command
}

fn run_command(command: &mut Command, label: &str) -> Result<()> {
    let output = command
        .output()
        .with_context(|| format!("failed to execute {label}"))?;

    if !output.status.success() {
        return Err(anyhow!(
            "{label} failed with status {}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    Ok(())
}

#[allow(dead_code)]
fn write_service_override_atomic(service: &ServiceRecord) -> Result<()> {
    let parent = service.override_path.parent().ok_or_else(|| {
        anyhow!(
            "systemd override path has no parent: {}",
            service.override_path.display()
        )
    })?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;

    let temporary = parent.join(format!(
        ".forkpkg.conf.tmp.{}.{}",
        std::process::id(),
        unix_time_secs()?
    ));
    let text = format!(
        "\
[Service]
ExecStart=
ExecStart={}
",
        service.exec_start
    );

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
        fs::rename(&temporary, &service.override_path).with_context(|| {
            format!(
                "failed to move {} to {}",
                temporary.display(),
                service.override_path.display()
            )
        })?;
        Ok(())
    })();

    if write_result.is_err() {
        let _ = fs::remove_file(&temporary);
    }

    write_result
}

fn systemctl_user<const N: usize>(args: [&str; N]) -> Result<()> {
    let output = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .output()
        .context("failed to execute systemctl --user")?;

    if !output.status.success() {
        return Err(anyhow!(
            "systemctl --user failed with status {}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ));
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

fn user_config_dir() -> Result<PathBuf> {
    Ok(match env::var_os("XDG_CONFIG_HOME").map(PathBuf::from) {
        Some(path) if path.is_absolute() => path,
        _ => home_dir()?.join(".config"),
    })
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
    let current_key = fork_key(metadata);
    let legacy_key = workspace::sanitize_workspace_name(&package_display_name(metadata));
    let package_key = package_key(metadata);
    let mut keys = vec![current_key];

    if metadata.fork_label() == workspace::DEFAULT_LABEL {
        if !keys.contains(&package_key) {
            keys.push(package_key);
        }
        if !keys.contains(&legacy_key) {
            keys.push(legacy_key);
        }
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

#[allow(dead_code)]
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

#[allow(dead_code)]
#[derive(Debug)]
enum EnableUndo {
    RemoveCreated {
        link: PathBuf,
    },
    RestorePrevious {
        link: PathBuf,
        backup: PathBuf,
    },
    RemoveCreatedServiceOverride {
        service: String,
        override_path: PathBuf,
    },
}

#[allow(dead_code)]
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
            EnableUndo::RemoveCreatedServiceOverride {
                service,
                override_path,
            } => {
                if link_exists(&override_path)? {
                    fs::remove_file(&override_path)
                        .with_context(|| format!("failed to remove {}", override_path.display()))?;
                }
                systemctl_user(["daemon-reload"])?;
                let _ = systemctl_user(["restart", service.as_str()]);
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
    RestoreServiceOverride {
        service: String,
        override_path: PathBuf,
        text: String,
    },
    ReinstallProfile {
        profile: Option<PathBuf>,
        store_path: PathBuf,
        priority: i64,
    },
    RestoreModule {
        module: PathBuf,
        text: String,
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
            DisableUndo::RestoreServiceOverride {
                service,
                override_path,
                text,
            } => {
                let parent = override_path.parent().ok_or_else(|| {
                    anyhow!(
                        "systemd override path has no parent: {}",
                        override_path.display()
                    )
                })?;
                fs::create_dir_all(parent)
                    .with_context(|| format!("failed to create {}", parent.display()))?;
                fs::write(&override_path, text)
                    .with_context(|| format!("failed to restore {}", override_path.display()))?;
                systemctl_user(["daemon-reload"])?;
                let _ = systemctl_user(["restart", service.as_str()]);
                Ok(())
            }
            DisableUndo::ReinstallProfile {
                profile,
                store_path,
                priority,
            } => nix_profile_add(profile.as_deref(), &store_path, priority),
            DisableUndo::RestoreModule { module, text } => write_text_atomic(&module, &text),
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

    use crate::metadata::{BaseMetadata, BuildMetadata, ForkMetadata, Metadata, PackageMetadata};

    use super::{
        ActivationPaths, ActivationRecordEntry, PreviousLink, apply_disable_record,
        apply_enable_record, executable_entries, list_record_entries_with_paths, package_key,
        plan_path_shim_with_paths, plan_systemd_user_service_with_paths, read_record,
        read_record_for_metadata,
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
    fn plans_systemd_user_service_activation_record() {
        let fixture = Fixture::new();
        let metadata = metadata_for("nixpkgs#portal", "portal", "x86_64-linux");
        let target = fixture.build_output.join("libexec/portal");
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        write_mode(&target, "#!/bin/sh\n", 0o755);
        let service_file = fixture
            .build_output
            .join("share/systemd/user/portal.service");
        fs::create_dir_all(service_file.parent().unwrap()).unwrap();
        fs::write(&service_file, "[Service]\n").unwrap();
        let exec_start = target.display().to_string();

        let record = plan_systemd_user_service_with_paths(
            &metadata,
            &fixture.workspace,
            &fixture.build_output,
            "portal.service",
            &service_file,
            &exec_start,
            &target,
            &fixture.paths,
        )
        .unwrap();

        assert_eq!(record.mode, "systemd-user-service");
        assert!(record.links.is_empty());
        assert_eq!(record.services.len(), 1);
        assert_eq!(record.services[0].service, "portal.service");
        assert_eq!(record.services[0].exec_start, exec_start);
        assert_eq!(record.services[0].target, target);
        assert!(record.services[0].target_blake3.is_some());
        assert_eq!(
            record.services[0].override_path,
            fixture
                .paths
                .user_config_dir
                .join("systemd/user/portal.service.d/forkpkg.conf")
        );
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
            let user_config = root.join("home/.config");
            let activations = root.join("state/forkpkg/activations");
            fs::create_dir_all(&source).unwrap();
            fs::create_dir_all(build_output.join("bin")).unwrap();
            fs::create_dir_all(&user_bin).unwrap();
            fs::create_dir_all(&user_config).unwrap();
            fs::create_dir_all(&activations).unwrap();
            fs::write(workspace_root.join("forkpkg.toml"), "").unwrap();

            Self {
                _temp: temp,
                paths: ActivationPaths {
                    activations_dir: activations,
                    user_bin_dir: user_bin,
                    user_config_dir: user_config,
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
            fork: ForkMetadata {
                label: "default".to_owned(),
            },
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
