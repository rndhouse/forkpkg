mod activation;
mod cli;
mod git;
mod metadata;
mod nix;
mod sharing;
mod targets;
mod workspace;

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};

use anyhow::{Context, Result};
use clap::Parser;
use serde::Serialize;

use crate::activation::{ActivationRecordEntry, PreviousLink};
use crate::cli::{ActivationBackend, Cli, Command};
use crate::metadata::{BaseMetadata, BuildMetadata, ForkMetadata, Metadata, PackageMetadata};

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Fork { installable, label } => fork(&installable, label.as_deref()),
        Command::List { json } => list(json),
        Command::Build { path, label } => build(path, label.as_deref()),
        Command::Apply { patch, path, label } => apply(patch, path, label.as_deref()),
        Command::Export {
            path,
            label,
            output,
        } => export(path, label.as_deref(), output),
        Command::Import {
            artifact,
            path,
            label,
        } => import(artifact, path, label.as_deref()),
        Command::Info { path, label } => info(path, label.as_deref()),
        Command::Targets { path, label, json } => targets(path, label.as_deref(), json),
        Command::Enable {
            path,
            label,
            backend,
            profile,
            switch,
            flake,
            dry_run,
        } => enable(
            path,
            label.as_deref(),
            backend,
            profile,
            switch,
            flake.as_deref(),
            dry_run,
        ),
        Command::Disable {
            path,
            label,
            dry_run,
        } => disable(path, label.as_deref(), dry_run),
        Command::DisableAll { dry_run } => disable_all(dry_run),
        Command::Doctor => doctor(),
        Command::Status { path, label } => status(path, label.as_deref()),
    }
}

fn fork(installable: &str, requested_label: Option<&str>) -> Result<()> {
    eprintln!("resolving {installable}");
    let resolved = nix::resolve_installable(installable)?;
    let workspace_name = workspace_name(&resolved);
    let existing = workspace::list_package(&workspace_name)?;
    let label = fork_label_or_default(requested_label, &workspace_name, existing.len())?;

    if requested_label.is_some()
        && label != workspace::DEFAULT_LABEL
        && workspace::legacy_workspace_exists(&workspace_name)?
    {
        let legacy_workspace =
            workspace::Workspace::new(workspace::legacy_workspace(&workspace_name)?);
        let legacy_metadata = Metadata::read(&legacy_workspace.metadata)?;
        if activation::status(&legacy_metadata)?.is_some() {
            anyhow::bail!(
                "legacy fork {workspace_name}/{} is active; disable it before creating another label",
                workspace::DEFAULT_LABEL
            );
        }
        eprintln!(
            "migrating legacy workspace for {workspace_name} to {workspace_name}/{}",
            workspace::DEFAULT_LABEL
        );
        workspace::migrate_legacy_to_default(&workspace_name)?;
    }

    if label == workspace::DEFAULT_LABEL && workspace::legacy_workspace_exists(&workspace_name)? {
        anyhow::bail!("fork workspace already exists: {workspace_name}/{label}");
    }

    let workspace_path = workspace::managed_workspace(&workspace_name, &label)?;
    if workspace_path.exists() {
        anyhow::bail!(
            "fork workspace already exists: {}",
            workspace_path.display()
        );
    }

    eprintln!("materializing post-patch source");
    let post_patch_source = nix::materialize_post_patch_source(&resolved.installable)?;
    let post_patch_info = nix::path_info(
        post_patch_source
            .to_str()
            .context("post-patch source path is not valid UTF-8")?,
    )?;

    eprintln!("creating workspace at {}", workspace_path.display());
    let workspace = workspace::create_managed(&workspace_name, &label)?;
    workspace::copy_tree(&post_patch_source, &workspace.source)?;

    let base_description = base_description(&resolved);
    eprintln!("initializing Git base commit");
    let base_commit = git::init_base_commit(
        &workspace.source,
        &format!("forkpkg base: {base_description}"),
    )?;

    let metadata = Metadata {
        format: 1,
        fork: ForkMetadata {
            label: label.clone(),
        },
        package: PackageMetadata {
            installable: resolved.installable.original.clone(),
            flake_ref: resolved.installable.flake_ref.clone(),
            attribute: resolved.installable.attribute.clone(),
            system: resolved.system.clone(),
            name: resolved.package_name.clone(),
            pname: resolved.package_pname.clone(),
            version: resolved.package_version.clone(),
        },
        base: BaseMetadata {
            nixpkgs_revision: resolved.flake.revision.clone(),
            nixpkgs_last_modified: resolved.flake.last_modified,
            nixpkgs_locked_nar_hash: resolved.flake.locked_nar_hash.clone(),
            nixpkgs_resolved_url: resolved.flake.resolved_url.clone(),
            nixpkgs_path: resolved.flake.path.clone(),
            derivation: resolved.derivation.clone(),
            output: resolved.output.clone(),
            source: resolved.source.clone(),
            source_revision: resolved.source_revision.clone(),
            source_hash: resolved
                .source_hash
                .clone()
                .or_else(|| resolved.source_store.as_ref().and_then(|info| info.nar_hash.clone())),
            source_ca: resolved.source_store.as_ref().and_then(|info| info.ca.clone()),
            post_patch_source: post_patch_source.display().to_string(),
            post_patch_source_hash: post_patch_info.and_then(|info| info.nar_hash),
            git_commit: base_commit.clone(),
        },
        build: BuildMetadata {
            strategy: "overrideAttrs-src-post-patch-tree".to_owned(),
            patch_handling: "local source is already post-patch; rebuild clears patches/prePatch/postPatch and replaces patchPhase with hooks only".to_owned(),
        },
    };

    metadata.write(&workspace.metadata)?;

    eprintln!("verifying unchanged fork builds");
    let output = nix::build_local_source(&metadata, &workspace.source)
        .with_context(|| format!("workspace remains at {}", workspace.root.display()))?;

    println!("workspace: {}", workspace.root.display());
    println!("fork: {}", fork_reference(&workspace_name, &label));
    println!("source: {}", workspace.source.display());
    println!("base_commit: {base_commit}");
    println!("output: {}", output.display());

    Ok(())
}

#[derive(Debug, Serialize)]
struct ListOutput {
    forks_dir: PathBuf,
    forks: Vec<ForkListEntry>,
}

#[derive(Debug, Serialize)]
struct ForkListEntry {
    package: String,
    label: String,
    reference: String,
    name: Option<String>,
    version: Option<String>,
    installable: String,
    attribute: String,
    system: String,
    active: bool,
    workspace: PathBuf,
    source: PathBuf,
    fork_point: ForkPoint,
    git: ForkGitState,
}

#[derive(Debug, Serialize)]
struct ForkPoint {
    git_commit: String,
    nixpkgs_revision: Option<String>,
    nixpkgs_resolved_url: Option<String>,
    nixpkgs_locked_nar_hash: Option<String>,
    derivation: String,
    original_output: String,
    original_source: Option<String>,
    source_revision: Option<String>,
    source_hash: Option<String>,
    post_patch_source: String,
    post_patch_source_hash: Option<String>,
}

#[derive(Debug, Serialize)]
struct ForkGitState {
    base_commit: String,
    base_commit_present: bool,
    head_commit: String,
    commits_on_top: Option<u64>,
    dirty: bool,
}

#[derive(Debug, Serialize)]
struct BackendReport {
    output: PathBuf,
    backends: Vec<BackendEntry>,
}

#[derive(Debug, Serialize)]
struct BackendEntry {
    id: String,
    kind: String,
    confidence: String,
    supported: bool,
    active: bool,
    evidence: Vec<String>,
    details: BackendDetails,
}

#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum BackendDetails {
    NixProfile {
        profile: Option<PathBuf>,
        profile_contains_output: bool,
    },
    NixosModule {
        module: Option<PathBuf>,
        overlay: Option<PathBuf>,
    },
    HomeManagerModule {
        module: Option<PathBuf>,
        overlay: Option<PathBuf>,
    },
    LegacyPathShim {
        executable_count: usize,
    },
    LegacySystemdUserService {
        service_count: usize,
    },
}

fn list(json: bool) -> Result<()> {
    let output = collect_list()?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&output).context("failed to serialize list JSON")?
        );
        return Ok(());
    }

    if output.forks.is_empty() {
        println!("no forks found");
        println!("directory: {}", output.forks_dir.display());
        return Ok(());
    }

    for fork in output.forks {
        println!(
            "{} {} active:{} commits_on_top:{} dirty:{}",
            fork.reference,
            fork.version.as_deref().unwrap_or("-"),
            yes_no(fork.active),
            fork.git
                .commits_on_top
                .map(|count| count.to_string())
                .unwrap_or_else(|| "unknown".to_owned()),
            yes_no(fork.git.dirty),
        );
        println!("  workspace: {}", fork.workspace.display());
        println!("  source: {}", fork.source.display());
        println!("  installable: {}", fork.installable);
        println!("  attribute: {}", fork.attribute);
        println!("  fork_point: {}", fork.fork_point.git_commit);
        println!("  head: {}", fork.git.head_commit);
        println!(
            "  base_commit_present: {}",
            yes_no(fork.git.base_commit_present)
        );
        if let Some(revision) = &fork.fork_point.nixpkgs_revision {
            println!("  nixpkgs_revision: {revision}");
        }
        if let Some(url) = &fork.fork_point.nixpkgs_resolved_url {
            println!("  nixpkgs_resolved_url: {url}");
        }
        if let Some(source) = &fork.fork_point.original_source {
            println!("  original_source: {source}");
        }
        if let Some(hash) = &fork.fork_point.source_hash {
            println!("  source_hash: {hash}");
        }
    }

    Ok(())
}

fn collect_list() -> Result<ListOutput> {
    let mut forks = Vec::new();
    for workspace in workspace::list_managed()? {
        let metadata = Metadata::read(&workspace.metadata)
            .with_context(|| format!("failed to read {}", workspace.metadata.display()))?;
        let repo =
            git::repo_state(&workspace.source, &metadata.base.git_commit).with_context(|| {
                format!(
                    "failed to inspect Git state in {}",
                    workspace.source.display()
                )
            })?;
        let active = activation::status(&metadata)?.is_some();
        let package = metadata
            .package
            .pname
            .clone()
            .or_else(|| metadata.package.name.clone())
            .unwrap_or_else(|| metadata.package.attribute.clone());
        let label = metadata.fork_label().to_owned();
        let reference = fork_reference(&package, &label);

        forks.push(ForkListEntry {
            package,
            label,
            reference,
            name: metadata.package.name,
            version: metadata.package.version,
            installable: metadata.package.installable,
            attribute: metadata.package.attribute,
            system: metadata.package.system,
            active,
            workspace: workspace.root,
            source: workspace.source,
            fork_point: ForkPoint {
                git_commit: metadata.base.git_commit,
                nixpkgs_revision: metadata.base.nixpkgs_revision,
                nixpkgs_resolved_url: metadata.base.nixpkgs_resolved_url,
                nixpkgs_locked_nar_hash: metadata.base.nixpkgs_locked_nar_hash,
                derivation: metadata.base.derivation,
                original_output: metadata.base.output,
                original_source: metadata.base.source,
                source_revision: metadata.base.source_revision,
                source_hash: metadata.base.source_hash,
                post_patch_source: metadata.base.post_patch_source,
                post_patch_source_hash: metadata.base.post_patch_source_hash,
            },
            git: ForkGitState {
                base_commit: repo.base_commit,
                base_commit_present: repo.base_commit_present,
                head_commit: repo.head_commit,
                commits_on_top: repo.commits_on_top,
                dirty: repo.dirty,
            },
        });
    }

    Ok(ListOutput {
        forks_dir: workspace::forks_dir()?,
        forks,
    })
}

fn build(path: Option<PathBuf>, label: Option<&str>) -> Result<()> {
    let workspace = workspace::resolve_labeled(path, label)?;
    let metadata = Metadata::read(&workspace.metadata)?;
    let output = nix::build_local_source(&metadata, &workspace.source)?;
    println!("{}", output.display());
    Ok(())
}

fn apply(patch: PathBuf, path: Option<PathBuf>, label: Option<&str>) -> Result<()> {
    let workspace = workspace::resolve_labeled(path, label)?;
    let metadata = Metadata::read(&workspace.metadata)?;
    let mut stdin_patch = None;
    let patch_path = if patch.as_os_str() == "-" {
        let mut file = tempfile::NamedTempFile::new()
            .context("failed to create temporary patch file for stdin")?;
        io::copy(&mut io::stdin().lock(), &mut file).context("failed to read patch from stdin")?;
        file.flush().context("failed to flush patch from stdin")?;
        let path = file.path().to_path_buf();
        stdin_patch = Some(file);
        path
    } else {
        patch
    };

    let summary = sharing::apply_patch(&workspace, &metadata, &patch_path)?;
    let patch_display = if stdin_patch.is_some() {
        "stdin".to_owned()
    } else {
        summary.patch.display().to_string()
    };

    println!("applied: {patch_display}");
    println!("method: {}", summary.method);
    println!("base_commit: {}", summary.base_commit);
    println!("head: {}", summary.head_commit);
    println!("commits_on_top: {}", summary.commits_on_top);

    Ok(())
}

fn export(path: Option<PathBuf>, label: Option<&str>, output: PathBuf) -> Result<()> {
    let workspace = workspace::resolve_labeled(path, label)?;
    let metadata = Metadata::read(&workspace.metadata)?;
    let summary = sharing::export_changes(&workspace, &metadata, &output)?;

    println!("exported: {}", summary.artifact.display());
    println!("base_commit: {}", summary.base_commit);
    println!("head: {}", summary.head_commit);
    println!("commits: {}", summary.commit_count);

    Ok(())
}

fn import(artifact: PathBuf, path: Option<PathBuf>, label: Option<&str>) -> Result<()> {
    let workspace = workspace::resolve_labeled(path, label)?;
    let metadata = Metadata::read(&workspace.metadata)?;
    let summary = sharing::import_changes(&workspace, &metadata, &artifact)?;

    println!("imported: {}", summary.artifact.display());
    println!("method: {}", summary.method);
    println!("base_commit: {}", summary.base_commit);
    println!("head: {}", summary.head_commit);
    println!("commits: {}", summary.commit_count);

    Ok(())
}

fn info(path: Option<PathBuf>, label: Option<&str>) -> Result<()> {
    let workspace = workspace::resolve_labeled(path, label)?;
    let metadata = Metadata::read(&workspace.metadata)?;
    let package = metadata
        .package
        .pname
        .as_deref()
        .or(metadata.package.name.as_deref())
        .unwrap_or(&metadata.package.attribute);
    let label = metadata.fork_label();

    print_optional("package", metadata.package.pname.as_deref());
    print_optional("name", metadata.package.name.as_deref());
    print_optional("version", metadata.package.version.as_deref());
    println!("label: {label}");
    println!("reference: {}", fork_reference(package, label));
    println!("installable: {}", metadata.package.installable);
    println!("attribute: {}", metadata.package.attribute);
    println!("system: {}", metadata.package.system);
    print_optional(
        "nixpkgs_revision",
        metadata.base.nixpkgs_revision.as_deref(),
    );
    print_optional(
        "nixpkgs_resolved_url",
        metadata.base.nixpkgs_resolved_url.as_deref(),
    );
    println!("drv: {}", metadata.base.derivation);
    println!("original_output: {}", metadata.base.output);
    print_optional("original_source", metadata.base.source.as_deref());
    print_optional("source_revision", metadata.base.source_revision.as_deref());
    print_optional("source_hash", metadata.base.source_hash.as_deref());
    println!("post_patch_source: {}", metadata.base.post_patch_source);
    println!("base_commit: {}", metadata.base.git_commit);
    println!("workspace: {}", workspace.root.display());
    println!("source: {}", workspace.source.display());

    Ok(())
}

fn targets(path: Option<PathBuf>, label: Option<&str>, json: bool) -> Result<()> {
    let workspace = workspace::resolve_labeled(path, label)?;
    let metadata = Metadata::read(&workspace.metadata)?;
    let output = nix::build_local_source(&metadata, &workspace.source)?;
    let report = discover_backends(&metadata, &output)?;

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).context("failed to serialize targets JSON")?
        );
        return Ok(());
    }

    println!("output: {}", report.output.display());
    if report.backends.is_empty() {
        println!("no Nix activation backends found");
        return Ok(());
    }

    for backend in &report.backends {
        println!(
            "{} kind:{} confidence:{} supported:{} active:{}",
            backend.id,
            backend.kind,
            backend.confidence,
            yes_no(backend.supported),
            yes_no(backend.active),
        );
        match &backend.details {
            BackendDetails::NixProfile {
                profile,
                profile_contains_output,
            } => {
                println!(
                    "  profile: {}",
                    profile
                        .as_ref()
                        .map(|path| path.display().to_string())
                        .unwrap_or_else(|| "default".to_owned())
                );
                println!(
                    "  profile_contains_output: {}",
                    yes_no(*profile_contains_output)
                );
            }
            BackendDetails::NixosModule { module, overlay }
            | BackendDetails::HomeManagerModule { module, overlay } => {
                if let Some(module) = module {
                    println!("  module: {}", module.display());
                }
                if let Some(overlay) = overlay {
                    println!("  overlay: {}", overlay.display());
                }
            }
            BackendDetails::LegacyPathShim { executable_count } => {
                println!("  executable_count: {executable_count}");
            }
            BackendDetails::LegacySystemdUserService { service_count } => {
                println!("  service_count: {service_count}");
            }
        }
        for evidence in &backend.evidence {
            println!("  evidence: {evidence}");
        }
    }

    Ok(())
}

fn enable(
    path: Option<PathBuf>,
    label: Option<&str>,
    requested_backend: ActivationBackend,
    profile: Option<PathBuf>,
    switch: bool,
    flake: Option<&str>,
    dry_run: bool,
) -> Result<()> {
    let workspace = workspace::resolve_labeled(path, label)?;
    let metadata = Metadata::read(&workspace.metadata)?;

    if dry_run {
        eprintln!("building fork to preview activation");
    } else {
        eprintln!("building fork before activation");
    }
    let output = nix::build_local_source(&metadata, &workspace.source)?;
    let backend = select_backend(requested_backend, &output)?;

    if profile.is_some() && backend != ActivationBackend::NixProfile {
        anyhow::bail!("--profile only applies to --backend nix-profile");
    }
    if flake.is_some() && !switch {
        anyhow::bail!("--flake only applies with --switch");
    }
    if switch && backend == ActivationBackend::NixProfile {
        anyhow::bail!(
            "--switch only applies to module backends; nix-profile activates immediately"
        );
    }
    if switch
        && matches!(
            backend,
            ActivationBackend::PathShim | ActivationBackend::SystemdUserService
        )
    {
        anyhow::bail!("--switch only applies to Nix module backends");
    }

    let record = match backend {
        ActivationBackend::Auto => unreachable!("auto backend should be resolved"),
        ActivationBackend::NixProfile if dry_run => {
            activation::plan_nix_profile(&metadata, &workspace, &output, profile.as_deref())?
        }
        ActivationBackend::NixProfile => {
            activation::enable_nix_profile(&metadata, &workspace, &output, profile.as_deref())?
        }
        ActivationBackend::NixosModule if dry_run => {
            activation::plan_nixos_module(&metadata, &workspace, &output)?
        }
        ActivationBackend::NixosModule => {
            activation::enable_nixos_module(&metadata, &workspace, &output)?
        }
        ActivationBackend::HomeManagerModule if dry_run => {
            activation::plan_home_manager_module(&metadata, &workspace, &output)?
        }
        ActivationBackend::HomeManagerModule => {
            activation::enable_home_manager_module(&metadata, &workspace, &output)?
        }
        ActivationBackend::PathShim if dry_run => {
            activation::plan_path_shim(&metadata, &workspace, &output)?
        }
        ActivationBackend::PathShim => {
            activation::enable_path_shim(&metadata, &workspace, &output)?
        }
        ActivationBackend::SystemdUserService => {
            enable_legacy_systemd_user_service(&metadata, &workspace, &output, dry_run)?
        }
    };

    if dry_run {
        println!("would enable: {}", record.package);
    } else {
        println!("enabled: {}", record.package);
    }
    println!("mode: {}", record.mode);
    println!("output: {}", record.build_output.display());
    print_legacy_link_records(&record, dry_run);
    print_service_records(&record, dry_run);
    print_profile_records(&record, dry_run);
    print_module_records(&record, dry_run);
    maybe_switch_backend(backend, switch, flake, dry_run)?;

    Ok(())
}

fn disable(path: Option<PathBuf>, label: Option<&str>, dry_run: bool) -> Result<()> {
    let workspace = workspace::resolve_labeled(path, label)?;
    let metadata = Metadata::read(&workspace.metadata)?;
    let record = activation::disable_plan(&metadata)?;
    if !dry_run {
        activation::disable_record(&record)?;
    }

    if dry_run {
        println!("would disable: {}", record.package);
    } else {
        println!("disabled: {}", record.package);
    }
    for link in &record.links {
        match &link.previous {
            PreviousLink::Absent => {
                if dry_run {
                    println!("would remove: {}", link.link.display());
                    println!("previous: absent");
                } else {
                    println!("removed: {}", link.link.display());
                }
            }
            PreviousLink::BackedUp { backup } => {
                if dry_run {
                    println!("would remove: {}", link.link.display());
                    println!(
                        "would restore: {} -> {}",
                        backup.display(),
                        link.link.display()
                    );
                } else {
                    println!("restored: {}", link.link.display());
                }
            }
        }
    }
    print_service_records(&record, dry_run);
    print_profile_records(&record, dry_run);
    print_module_records(&record, dry_run);

    Ok(())
}

fn enable_legacy_systemd_user_service(
    metadata: &Metadata,
    workspace: &workspace::Workspace,
    output: &Path,
    dry_run: bool,
) -> Result<activation::ActivationRecord> {
    let report = targets::discover(output)?;
    let target = select_systemd_user_service_target(&report)?;
    let spec = targets::systemd_user_service_spec(target)?;
    let executable = spec
        .executable
        .as_ref()
        .context("systemd user service target has no absolute executable")?;

    if !executable.starts_with(output) {
        anyhow::bail!(
            "refusing service activation because target executable is outside the forked output: {}",
            executable.display()
        );
    }

    if dry_run {
        activation::plan_systemd_user_service(
            metadata,
            workspace,
            output,
            &spec.service,
            &spec.service_file,
            &spec.exec_start,
            executable,
        )
    } else {
        activation::enable_systemd_user_service(
            metadata,
            workspace,
            output,
            &spec.service,
            &spec.service_file,
            &spec.exec_start,
            executable,
        )
    }
}

fn select_systemd_user_service_target(
    report: &targets::TargetReport,
) -> Result<&targets::ActivationTarget> {
    let supported = report
        .targets
        .iter()
        .filter(|target| target.supported && target.kind == "systemd-user-service")
        .collect::<Vec<_>>();

    match supported.as_slice() {
        [target] => Ok(*target),
        [] => anyhow::bail!("no supported systemd user service target found"),
        _ => {
            let ids = supported
                .iter()
                .map(|target| target.id.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            anyhow::bail!("multiple systemd user service targets found ({ids})")
        }
    }
}

fn disable_all(dry_run: bool) -> Result<()> {
    let entries = activation::list_record_entries()?;
    if entries.is_empty() {
        println!("no active forks");
        return Ok(());
    }

    let mut failures = 0usize;
    for entry in entries {
        match entry {
            ActivationRecordEntry::Valid { record, .. } => {
                if dry_run {
                    println!("would disable: {}", record.package);
                    print_disable_record_links(&record, true);
                    continue;
                }

                match activation::disable_record(&record) {
                    Ok(()) => {
                        println!("disabled: {}", record.package);
                        print_disable_record_links(&record, false);
                    }
                    Err(error) => {
                        failures += 1;
                        eprintln!("failed to disable {}: {error:#}", record.package);
                    }
                }
            }
            ActivationRecordEntry::Broken { path, problem } => {
                failures += 1;
                eprintln!(
                    "failed to read activation record {}: {problem}",
                    path.display()
                );
            }
        }
    }

    if failures > 0 {
        anyhow::bail!("{failures} activation(s) could not be disabled");
    }

    Ok(())
}

fn doctor() -> Result<()> {
    let forks = workspace::list_managed()?;
    println!("forks_dir: {}", workspace::forks_dir()?.display());
    println!("managed_forks: {}", forks.len());
    let mut problems = 0usize;
    for workspace in &forks {
        match Metadata::read(&workspace.metadata) {
            Ok(metadata) => {
                let package = activation::fork_display_name(&metadata);
                match activation::status(&metadata) {
                    Ok(active) => {
                        println!(
                            "fork: {} active:{} workspace:{}",
                            package,
                            yes_no(active.is_some()),
                            workspace.root.display()
                        );
                    }
                    Err(error) => {
                        problems += 1;
                        println!(
                            "fork: {} active:broken workspace:{} problem:{}",
                            package,
                            workspace.root.display(),
                            error
                        );
                    }
                }
            }
            Err(error) => {
                problems += 1;
                println!(
                    "fork: unknown status:broken workspace:{} problem:{}",
                    workspace.root.display(),
                    error
                );
            }
        }
    }

    let records = activation::list_record_entries()?;
    println!(
        "activations_dir: {}",
        activation::activations_dir()?.display()
    );
    println!("activation_records: {}", records.len());

    for entry in records {
        match entry {
            ActivationRecordEntry::Valid { record, .. } => {
                let check = activation::check_record(*record);
                if check.is_ok() {
                    println!("activation: {} status:ok", check.record.package);
                    for link in &check.record.links {
                        println!(
                            "  link: {} -> {}",
                            link.link.display(),
                            link.target.display()
                        );
                        print_indented_target_blake3(link, true);
                    }
                    for service in &check.record.services {
                        println!(
                            "  service: {} override:{}",
                            service.service,
                            service.override_path.display()
                        );
                        match &service.target_blake3 {
                            Some(hash) => {
                                println!("  target_blake3: {hash} verified:yes");
                            }
                            None => println!("  target_blake3: missing verified:no"),
                        }
                    }
                    for profile in &check.record.profiles {
                        println!(
                            "  profile: {} store_path:{}",
                            profile_display(profile.profile.as_ref()),
                            profile.store_path.display()
                        );
                    }
                    for module in &check.record.modules {
                        println!(
                            "  module: {} backend:{} overlay:{}",
                            module.module.display(),
                            module.backend,
                            module.overlay.display()
                        );
                    }
                } else {
                    problems += check.problems.len();
                    println!("activation: {} status:broken", check.record.package);
                    for problem in &check.problems {
                        println!("  problem: {problem}");
                    }
                }
            }
            ActivationRecordEntry::Broken { path, problem } => {
                problems += 1;
                println!("activation_record: {} status:broken", path.display());
                println!("  problem: {problem}");
            }
        }
    }

    if problems == 0 {
        println!("doctor: ok");
    } else {
        println!("doctor: {problems} problem(s)");
        anyhow::bail!("doctor found {problems} problem(s)");
    }

    Ok(())
}

fn status(path: Option<PathBuf>, label: Option<&str>) -> Result<()> {
    let workspace = workspace::resolve_labeled(path, label)?;
    let metadata = Metadata::read(&workspace.metadata)?;
    let repo = git::repo_state(&workspace.source, &metadata.base.git_commit)?;
    let package = metadata
        .package
        .pname
        .as_deref()
        .or(metadata.package.name.as_deref())
        .unwrap_or(&metadata.package.attribute);
    let label = metadata.fork_label();
    let reference = fork_reference(package, label);

    match activation::status(&metadata)? {
        Some(record) => {
            let check = activation::check_record(record.clone());
            println!("fork: {reference}");
            if check.is_ok() {
                println!("active: yes");
                println!("verified: yes");
            } else {
                println!("active: broken");
                println!("verified: no");
                for problem in &check.problems {
                    println!("reason: {problem}");
                }
            }
            println!("mode: {}", record.mode);
            println!("output: {}", record.build_output.display());
            for target_id in record_target_ids(&record) {
                println!("target: {target_id}");
            }
            for link in &record.links {
                println!(
                    "binary: {} -> {}",
                    link.link.display(),
                    link.target.display()
                );
                print_target_blake3(link);
                match &link.previous {
                    PreviousLink::Absent => println!("previous: absent"),
                    PreviousLink::BackedUp { backup } => {
                        println!("previous_backup: {}", backup.display());
                    }
                }
            }
            for service in &record.services {
                println!("service: {}", service.service);
                println!("service_file: {}", service.service_file.display());
                println!("override: {}", service.override_path.display());
                println!("exec_start: {}", service.exec_start);
                println!("target: {}", service.target.display());
                match &service.target_blake3 {
                    Some(hash) => println!("target_blake3: {hash}"),
                    None => println!("target_blake3: missing"),
                }
            }
            for profile in &record.profiles {
                println!("profile: {}", profile_display(profile.profile.as_ref()));
                println!("profile_store_path: {}", profile.store_path.display());
                if let Some(element) = &profile.element {
                    println!("profile_element: {element}");
                }
                println!("profile_priority: {}", profile.priority);
            }
            for module in &record.modules {
                println!("module_backend: {}", module.backend);
                println!("module: {}", module.module.display());
                println!("overlay: {}", module.overlay.display());
            }
        }
        None => {
            println!("fork: {reference}");
            println!("active: no");
        }
    }
    println!("workspace: {}", workspace.root.display());
    println!("source: {}", workspace.source.display());
    println!("fork_point: {}", repo.base_commit);
    println!("head: {}", repo.head_commit);
    println!(
        "commits_on_top: {}",
        repo.commits_on_top
            .map(|count| count.to_string())
            .unwrap_or_else(|| "unknown".to_owned())
    );
    println!("dirty: {}", yes_no(repo.dirty));
    println!("base_commit_present: {}", yes_no(repo.base_commit_present));

    Ok(())
}

fn print_disable_record_links(record: &activation::ActivationRecord, dry_run: bool) {
    for link in &record.links {
        match &link.previous {
            PreviousLink::Absent => {
                if dry_run {
                    println!("would remove: {}", link.link.display());
                    println!("previous: absent");
                } else {
                    println!("removed: {}", link.link.display());
                }
            }
            PreviousLink::BackedUp { backup } => {
                if dry_run {
                    println!("would remove: {}", link.link.display());
                    println!(
                        "would restore: {} -> {}",
                        backup.display(),
                        link.link.display()
                    );
                } else {
                    println!("restored: {}", link.link.display());
                }
            }
        }
    }
    print_service_records(record, dry_run);
    print_profile_records(record, dry_run);
    print_module_records(record, dry_run);
}

fn print_service_records(record: &activation::ActivationRecord, dry_run: bool) {
    for service in &record.services {
        if dry_run {
            println!("override: {}", service.override_path.display());
            println!("would run: systemctl --user daemon-reload");
            println!("would run: systemctl --user restart {}", service.service);
        } else {
            println!("service: {}", service.service);
            println!("override: {}", service.override_path.display());
        }
        println!("service_file: {}", service.service_file.display());
        println!("exec_start: {}", service.exec_start);
        println!("target: {}", service.target.display());
        match &service.target_blake3 {
            Some(hash) => println!("target_blake3: {hash}"),
            None => println!("target_blake3: missing"),
        }
    }
}

fn print_legacy_link_records(record: &activation::ActivationRecord, dry_run: bool) {
    for link in &record.links {
        match &link.previous {
            PreviousLink::Absent => {
                if dry_run {
                    println!(
                        "would create: {} -> {}",
                        link.link.display(),
                        link.target.display()
                    );
                    print_target_blake3(link);
                    println!("previous: absent");
                } else {
                    println!("link: {} -> {}", link.link.display(), link.target.display());
                    print_target_blake3(link);
                }
            }
            PreviousLink::BackedUp { backup } => {
                if dry_run {
                    println!("would move existing: {}", link.link.display());
                    println!("backup: {}", backup.display());
                    println!(
                        "would create: {} -> {}",
                        link.link.display(),
                        link.target.display()
                    );
                    print_target_blake3(link);
                } else {
                    println!("link: {} -> {}", link.link.display(), link.target.display());
                    print_target_blake3(link);
                    println!("previous backup: {}", backup.display());
                }
            }
        }
    }
    if !record.links.is_empty()
        && let Ok(Some(hint)) = activation::path_hint()
    {
        println!("warning: {hint}");
    }
}

fn print_profile_records(record: &activation::ActivationRecord, _dry_run: bool) {
    for profile in &record.profiles {
        println!("profile: {}", profile_display(profile.profile.as_ref()));
        println!("profile_store_path: {}", profile.store_path.display());
        if let Some(element) = &profile.element {
            println!("profile_element: {element}");
        }
        println!("profile_priority: {}", profile.priority);
    }
}

fn print_module_records(record: &activation::ActivationRecord, dry_run: bool) {
    for module in &record.modules {
        if dry_run {
            println!("would write module: {}", module.module.display());
            println!("would write overlay: {}", module.overlay.display());
        } else {
            println!("module_backend: {}", module.backend);
            println!("module: {}", module.module.display());
            println!("overlay: {}", module.overlay.display());
        }
        match module.backend.as_str() {
            "nixos-module" => {
                println!("next: import module in NixOS config, then run nixos-rebuild switch");
            }
            "home-manager-module" => {
                println!(
                    "next: import module in Home Manager config, then run home-manager switch"
                );
            }
            _ => {}
        }
    }
}

fn maybe_switch_backend(
    backend: ActivationBackend,
    switch: bool,
    flake: Option<&str>,
    dry_run: bool,
) -> Result<()> {
    if !switch {
        return Ok(());
    }

    let command = switch_command(backend, flake)?;
    if dry_run {
        println!("would run: {}", command.display());
        return Ok(());
    }

    eprintln!("running {}", command.display());
    command.run()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SwitchCommand {
    program: String,
    args: Vec<String>,
}

impl SwitchCommand {
    fn display(&self) -> String {
        std::iter::once(self.program.as_str())
            .chain(self.args.iter().map(String::as_str))
            .map(shell_display_word)
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn run(&self) -> Result<()> {
        let status = ProcessCommand::new(&self.program)
            .args(&self.args)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .with_context(|| format!("failed to execute {}", self.program))?;

        if !status.success() {
            anyhow::bail!("{} failed with status {}", self.display(), status);
        }

        Ok(())
    }
}

fn switch_command(backend: ActivationBackend, flake: Option<&str>) -> Result<SwitchCommand> {
    let mut command = match backend {
        ActivationBackend::NixosModule => SwitchCommand {
            program: "sudo".to_owned(),
            args: vec!["nixos-rebuild".to_owned(), "switch".to_owned()],
        },
        ActivationBackend::HomeManagerModule => SwitchCommand {
            program: "home-manager".to_owned(),
            args: vec!["switch".to_owned()],
        },
        ActivationBackend::Auto
        | ActivationBackend::NixProfile
        | ActivationBackend::PathShim
        | ActivationBackend::SystemdUserService => {
            anyhow::bail!("backend has no switch command: {backend:?}")
        }
    };

    if let Some(flake) = flake {
        command.args.push("--flake".to_owned());
        command.args.push(flake.to_owned());
    }

    Ok(command)
}

fn shell_display_word(word: &str) -> String {
    if word
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '/' | '.' | '_' | '-' | ':' | '#'))
    {
        return word.to_owned();
    }

    format!("'{}'", word.replace('\'', "'\\''"))
}

fn record_target_ids(record: &activation::ActivationRecord) -> Vec<String> {
    let mut ids = Vec::new();
    if !record.links.is_empty() {
        ids.push("path-shim".to_owned());
    }
    for service in &record.services {
        ids.push(format!("systemd-user:{}", service.service));
    }
    if !record.profiles.is_empty() {
        ids.push("nix-profile".to_owned());
    }
    for module in &record.modules {
        ids.push(module.backend.clone());
    }
    ids
}

fn select_backend(requested: ActivationBackend, output: &Path) -> Result<ActivationBackend> {
    if requested != ActivationBackend::Auto {
        return Ok(requested);
    }

    if is_nixos() && output_looks_service_managed(output)? {
        return Ok(ActivationBackend::NixosModule);
    }

    Ok(ActivationBackend::NixProfile)
}

fn discover_backends(metadata: &Metadata, output: &Path) -> Result<BackendReport> {
    let active = activation::status(metadata)?;
    let active_mode = active.as_ref().map(|record| record.mode.as_str());
    let target_report = targets::discover(output)?;
    let service_count = systemd_user_service_target_count(&target_report);
    let service_managed = target_report.targets.iter().any(|target| {
        matches!(
            target.details,
            targets::TargetDetails::SystemdUserService { .. }
                | targets::TargetDetails::DbusService { .. }
        )
    });
    let executable_count = output_bin_executable_count(output)?;
    let has_profile_files = output.join("bin").is_dir()
        || output.join("share/applications").is_dir()
        || output.join("share").is_dir();
    let profile_contains_output =
        activation::profile_contains_store_path(None, output).unwrap_or(false);

    let mut backends = Vec::new();
    let mut profile_evidence = vec![
        "Nix profile can install the built store output directly".to_owned(),
        "Nix owns the profile symlink tree and profile generations".to_owned(),
    ];
    if has_profile_files {
        profile_evidence.push("output contains files that profiles normally expose".to_owned());
    } else {
        profile_evidence.push("output has no obvious profile-facing bin/share files".to_owned());
    }
    if service_managed {
        profile_evidence
            .push("service-like outputs may need a NixOS/Home Manager module instead".to_owned());
    }
    backends.push(BackendEntry {
        id: "nix-profile".to_owned(),
        kind: "nix-profile".to_owned(),
        confidence: if service_managed {
            "low"
        } else if has_profile_files {
            "high"
        } else {
            "medium"
        }
        .to_owned(),
        supported: true,
        active: active_mode == Some("nix-profile") || profile_contains_output,
        evidence: profile_evidence,
        details: BackendDetails::NixProfile {
            profile: None,
            profile_contains_output,
        },
    });

    let nixos_supported = is_nixos();
    let mut nixos_evidence = vec![
        "generated module adds a nixpkgs overlay for the forked package".to_owned(),
        "NixOS activation happens through the user's normal nixos-rebuild switch".to_owned(),
    ];
    if service_managed {
        nixos_evidence.push("output contains service or D-Bus activation files".to_owned());
    }
    if !nixos_supported {
        nixos_evidence.push("this machine does not look like NixOS".to_owned());
    }
    let (nixos_module, nixos_overlay) = active
        .as_ref()
        .and_then(|record| {
            record
                .modules
                .iter()
                .find(|module| module.backend == "nixos-module")
        })
        .map(|module| (Some(module.module.clone()), Some(module.overlay.clone())))
        .unwrap_or((None, None));
    backends.push(BackendEntry {
        id: "nixos-module".to_owned(),
        kind: "nixos-module".to_owned(),
        confidence: if nixos_supported && service_managed {
            "high"
        } else if nixos_supported {
            "medium"
        } else {
            "low"
        }
        .to_owned(),
        supported: nixos_supported,
        active: active_mode == Some("nixos-module"),
        evidence: nixos_evidence,
        details: BackendDetails::NixosModule {
            module: nixos_module,
            overlay: nixos_overlay,
        },
    });

    let home_manager_supported = command_exists("home-manager");
    let mut home_evidence = vec![
        "generated module adds a nixpkgs overlay for Home Manager evaluation".to_owned(),
        "activation happens through the user's normal home-manager switch".to_owned(),
    ];
    if !home_manager_supported {
        home_evidence.push("home-manager command was not found in PATH".to_owned());
    }
    let (home_module, home_overlay) = active
        .as_ref()
        .and_then(|record| {
            record
                .modules
                .iter()
                .find(|module| module.backend == "home-manager-module")
        })
        .map(|module| (Some(module.module.clone()), Some(module.overlay.clone())))
        .unwrap_or((None, None));
    backends.push(BackendEntry {
        id: "home-manager-module".to_owned(),
        kind: "home-manager-module".to_owned(),
        confidence: if home_manager_supported {
            "medium"
        } else {
            "low"
        }
        .to_owned(),
        supported: home_manager_supported,
        active: active_mode == Some("home-manager-module"),
        evidence: home_evidence,
        details: BackendDetails::HomeManagerModule {
            module: home_module,
            overlay: home_overlay,
        },
    });

    backends.push(BackendEntry {
        id: "path-shim".to_owned(),
        kind: "path-shim".to_owned(),
        confidence: if executable_count > 0 { "low" } else { "none" }.to_owned(),
        supported: executable_count > 0,
        active: active_mode == Some("path-shim"),
        evidence: vec![
            "legacy direct activation; bypasses Nix profile/module ownership".to_owned(),
            format!("output has {executable_count} executable(s) in bin/"),
        ],
        details: BackendDetails::LegacyPathShim { executable_count },
    });

    backends.push(BackendEntry {
        id: "systemd-user-service".to_owned(),
        kind: "systemd-user-service".to_owned(),
        confidence: if service_count == 1 { "low" } else { "none" }.to_owned(),
        supported: service_count == 1,
        active: active_mode == Some("systemd-user-service"),
        evidence: vec![
            "legacy direct activation; writes a user systemd override".to_owned(),
            format!("output has {service_count} supported systemd user service target(s)"),
        ],
        details: BackendDetails::LegacySystemdUserService { service_count },
    });

    Ok(BackendReport {
        output: output.to_path_buf(),
        backends,
    })
}

fn output_looks_service_managed(output: &Path) -> Result<bool> {
    let report = targets::discover(output)?;
    Ok(report.targets.iter().any(|target| {
        matches!(
            target.details,
            targets::TargetDetails::SystemdUserService { .. }
                | targets::TargetDetails::DbusService { .. }
        )
    }))
}

fn systemd_user_service_target_count(report: &targets::TargetReport) -> usize {
    report
        .targets
        .iter()
        .filter(|target| target.supported && target.kind == "systemd-user-service")
        .count()
}

fn output_bin_executable_count(output: &Path) -> Result<usize> {
    let bin_dir = output.join("bin");
    if !bin_dir.is_dir() {
        return Ok(0);
    }

    let mut count = 0usize;
    for entry in std::fs::read_dir(&bin_dir)
        .with_context(|| format!("failed to read {}", bin_dir.display()))?
    {
        let entry =
            entry.with_context(|| format!("failed to read entry in {}", bin_dir.display()))?;
        let file_type = entry
            .file_type()
            .with_context(|| format!("failed to inspect {}", entry.path().display()))?;
        if !(file_type.is_file() || file_type.is_symlink()) {
            continue;
        }
        let metadata = std::fs::metadata(entry.path())
            .with_context(|| format!("failed to inspect {}", entry.path().display()))?;
        if metadata.is_file()
            && std::os::unix::fs::PermissionsExt::mode(&metadata.permissions()) & 0o111 != 0
        {
            count += 1;
        }
    }

    Ok(count)
}

fn is_nixos() -> bool {
    Path::new("/run/current-system").exists() || Path::new("/etc/NIXOS").exists()
}

fn command_exists(name: &str) -> bool {
    std::process::Command::new("sh")
        .arg("-c")
        .arg("command -v \"$1\" >/dev/null 2>&1")
        .arg("sh")
        .arg(name)
        .status()
        .is_ok_and(|status| status.success())
}

fn profile_display(profile: Option<&PathBuf>) -> String {
    profile
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "default".to_owned())
}

fn fork_label_or_default(
    requested_label: Option<&str>,
    package: &str,
    existing_count: usize,
) -> Result<String> {
    match requested_label {
        Some(label) => Ok(workspace::sanitize_workspace_name(label)),
        None if existing_count == 0 => Ok(workspace::DEFAULT_LABEL.to_owned()),
        None => anyhow::bail!(
            "fork already exists for {package}; use --label <label> to create another fork"
        ),
    }
}

fn fork_reference(package: &str, label: &str) -> String {
    let package = workspace::sanitize_workspace_name(package);
    let label = workspace::sanitize_workspace_name(label);
    if label == workspace::DEFAULT_LABEL {
        package
    } else {
        format!("{package} --label {label}")
    }
}

fn workspace_name(resolved: &nix::ResolvedPackage) -> String {
    let display_name = resolved
        .package_pname
        .as_deref()
        .or_else(|| resolved.installable.attr_path.last().map(String::as_str))
        .or(resolved.package_name.as_deref())
        .unwrap_or("package")
        .to_owned();
    workspace::sanitize_workspace_name(&display_name)
}

fn base_description(resolved: &nix::ResolvedPackage) -> String {
    if let Some(revision) = &resolved.flake.revision {
        format!(
            "{}@{}#{}",
            resolved.installable.flake_ref, revision, resolved.installable.attribute
        )
    } else {
        resolved.installable.original.clone()
    }
}

fn print_optional(label: &str, value: Option<&str>) {
    if let Some(value) = value {
        println!("{label}: {value}");
    }
}

fn print_target_blake3(link: &activation::LinkRecord) {
    match &link.target_blake3 {
        Some(hash) => println!("target_blake3: {hash}"),
        None => println!("target_blake3: missing"),
    }
}

fn print_indented_target_blake3(link: &activation::LinkRecord, verified: bool) {
    match &link.target_blake3 {
        Some(hash) => println!("  target_blake3: {hash} verified:{}", yes_no(verified)),
        None => println!("  target_blake3: missing verified:no"),
    }
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

#[cfg(test)]
mod tests {
    use super::{ActivationBackend, switch_command};

    #[test]
    fn nixos_switch_command_uses_sudo_and_optional_flake() {
        let command = switch_command(
            ActivationBackend::NixosModule,
            Some("/home/user/nixos#tower"),
        )
        .unwrap();

        assert_eq!(command.program, "sudo");
        assert_eq!(
            command.args,
            [
                "nixos-rebuild",
                "switch",
                "--flake",
                "/home/user/nixos#tower"
            ]
        );
    }

    #[test]
    fn home_manager_switch_command_does_not_use_sudo() {
        let command = switch_command(ActivationBackend::HomeManagerModule, None).unwrap();

        assert_eq!(command.program, "home-manager");
        assert_eq!(command.args, ["switch"]);
    }
}
