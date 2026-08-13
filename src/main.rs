mod activation;
mod cli;
mod git;
mod metadata;
mod nix;
mod sharing;
mod targets;
mod workspace;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use serde::Serialize;

use crate::activation::{ActivationRecordEntry, PreviousLink};
use crate::cli::{Cli, Command};
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
        Command::Build { path } => build(path),
        Command::Export { path, output } => export(path, output),
        Command::Import { artifact, path } => import(artifact, path),
        Command::Info { path } => info(path),
        Command::Targets { path, json } => targets(path, json),
        Command::Enable {
            path,
            target,
            dry_run,
        } => enable(path, target.as_deref(), dry_run),
        Command::Disable {
            path,
            target,
            dry_run,
        } => disable(path, target.as_deref(), dry_run),
        Command::DisableAll { dry_run } => disable_all(dry_run),
        Command::Doctor => doctor(),
        Command::Status { path } => status(path),
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
    println!("fork: {workspace_name}/{label}");
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

fn build(path: Option<PathBuf>) -> Result<()> {
    let workspace = workspace::resolve(path)?;
    let metadata = Metadata::read(&workspace.metadata)?;
    let output = nix::build_local_source(&metadata, &workspace.source)?;
    println!("{}", output.display());
    Ok(())
}

fn export(path: Option<PathBuf>, output: PathBuf) -> Result<()> {
    let workspace = workspace::resolve(path)?;
    let metadata = Metadata::read(&workspace.metadata)?;
    let summary = sharing::export_changes(&workspace, &metadata, &output)?;

    println!("exported: {}", summary.artifact.display());
    println!("base_commit: {}", summary.base_commit);
    println!("head: {}", summary.head_commit);
    println!("commits: {}", summary.commit_count);

    Ok(())
}

fn import(artifact: PathBuf, path: Option<PathBuf>) -> Result<()> {
    let workspace = workspace::resolve(path)?;
    let metadata = Metadata::read(&workspace.metadata)?;
    let summary = sharing::import_changes(&workspace, &metadata, &artifact)?;

    println!("imported: {}", summary.artifact.display());
    println!("method: {}", summary.method);
    println!("base_commit: {}", summary.base_commit);
    println!("head: {}", summary.head_commit);
    println!("commits: {}", summary.commit_count);

    Ok(())
}

fn info(path: Option<PathBuf>) -> Result<()> {
    let workspace = workspace::resolve(path)?;
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

fn targets(path: Option<PathBuf>, json: bool) -> Result<()> {
    let workspace = workspace::resolve(path)?;
    let metadata = Metadata::read(&workspace.metadata)?;
    let output = nix::build_local_source(&metadata, &workspace.source)?;
    let mut report = targets::discover(&output)?;
    mark_active_targets(&metadata, &mut report)?;

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).context("failed to serialize targets JSON")?
        );
        return Ok(());
    }

    println!("output: {}", report.output.display());
    if report.targets.is_empty() {
        println!("no activation targets found");
        return Ok(());
    }

    for target in &report.targets {
        println!(
            "{} kind:{} confidence:{} supported:{} active:{}",
            target.id,
            target.kind,
            target.confidence,
            yes_no(target.supported),
            yes_no(target.active),
        );
        match &target.details {
            targets::TargetDetails::PathShim {
                executables,
                path_matches,
            } => {
                for executable in executables {
                    println!("  executable: {}", executable.display());
                }
                for path_match in path_matches {
                    println!(
                        "  path_match: {} resolved:{} points_to_output:{}",
                        path_match.path.display(),
                        path_match
                            .resolved
                            .as_ref()
                            .map(|path| path.display().to_string())
                            .unwrap_or_else(|| "-".to_owned()),
                        yes_no(path_match.points_to_output),
                    );
                }
            }
            targets::TargetDetails::SystemdUserService {
                service,
                service_file,
                exec_start,
                executable,
                dbus_names,
            } => {
                println!("  service: {service}");
                println!("  service_file: {}", service_file.display());
                println!("  exec_start: {exec_start}");
                if let Some(executable) = executable {
                    println!("  executable: {}", executable.display());
                }
                for dbus_name in dbus_names {
                    println!("  dbus_name: {dbus_name}");
                }
            }
            targets::TargetDetails::DbusService {
                name,
                service_file,
                exec,
                systemd_service,
            } => {
                println!("  name: {name}");
                println!("  service_file: {}", service_file.display());
                if let Some(exec) = exec {
                    println!("  exec: {exec}");
                }
                if let Some(systemd_service) = systemd_service {
                    println!("  systemd_service: {systemd_service}");
                }
            }
        }
        for evidence in &target.evidence {
            println!("  evidence: {evidence}");
        }
    }

    Ok(())
}

fn enable(path: Option<PathBuf>, requested_target: Option<&str>, dry_run: bool) -> Result<()> {
    let workspace = workspace::resolve(path)?;
    let metadata = Metadata::read(&workspace.metadata)?;

    if dry_run {
        eprintln!("building fork to preview activation");
    } else {
        eprintln!("building fork before activation");
    }
    let output = nix::build_local_source(&metadata, &workspace.source)?;
    let mut report = targets::discover(&output)?;
    mark_active_targets(&metadata, &mut report)?;
    let target = select_target(&report, requested_target)?;

    match target.kind.as_str() {
        "path-shim" => enable_path_shim_target(&metadata, &workspace, &output, dry_run),
        "systemd-user-service" => {
            enable_systemd_user_target(&metadata, &workspace, &output, target, dry_run)
        }
        other => anyhow::bail!("activation target kind is not supported yet: {other}"),
    }
}

fn enable_path_shim_target(
    metadata: &Metadata,
    workspace: &workspace::Workspace,
    output: &std::path::Path,
    dry_run: bool,
) -> Result<()> {
    let record = if dry_run {
        activation::plan_path_shim(metadata, workspace, output)?
    } else {
        activation::enable_path_shim(metadata, workspace, output)?
    };

    if dry_run {
        println!("would enable: {}", record.package);
    } else {
        println!("enabled: {}", record.package);
    }
    println!("mode: {}", record.mode);
    println!("output: {}", record.build_output.display());
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
    if let Some(hint) = activation::path_hint()? {
        println!("warning: {hint}");
    }

    Ok(())
}

fn enable_systemd_user_target(
    metadata: &Metadata,
    workspace: &workspace::Workspace,
    output: &std::path::Path,
    target: &targets::ActivationTarget,
    dry_run: bool,
) -> Result<()> {
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

    let record = if dry_run {
        activation::plan_systemd_user_service(
            metadata,
            workspace,
            output,
            &spec.service,
            &spec.service_file,
            &spec.exec_start,
            executable,
        )?
    } else {
        activation::enable_systemd_user_service(
            metadata,
            workspace,
            output,
            &spec.service,
            &spec.service_file,
            &spec.exec_start,
            executable,
        )?
    };

    if dry_run {
        println!("would enable: {}", target.id);
    } else {
        println!("enabled: {}", target.id);
    }
    println!("mode: {}", record.mode);
    println!("output: {}", record.build_output.display());
    print_service_records(&record, dry_run);

    Ok(())
}

fn disable(path: Option<PathBuf>, requested_target: Option<&str>, dry_run: bool) -> Result<()> {
    let workspace = workspace::resolve(path)?;
    let metadata = Metadata::read(&workspace.metadata)?;
    let record = activation::disable_plan(&metadata)?;
    ensure_record_matches_requested_target(&record, requested_target)?;
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

    Ok(())
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

fn status(path: Option<PathBuf>) -> Result<()> {
    let workspace = workspace::resolve(path)?;
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

fn mark_active_targets(metadata: &Metadata, report: &mut targets::TargetReport) -> Result<()> {
    let Some(record) = activation::status(metadata)? else {
        return Ok(());
    };
    let active_target_ids = record_target_ids(&record);
    for target in &mut report.targets {
        target.active = active_target_ids.iter().any(|id| id == &target.id);
    }
    Ok(())
}

fn ensure_record_matches_requested_target(
    record: &activation::ActivationRecord,
    requested_target: Option<&str>,
) -> Result<()> {
    let Some(requested_target) = requested_target else {
        return Ok(());
    };
    let target_ids = record_target_ids(record);
    if target_ids.iter().any(|id| id == requested_target) {
        return Ok(());
    }
    anyhow::bail!(
        "active record target mismatch; active target(s): {}",
        target_ids.join(", ")
    )
}

fn record_target_ids(record: &activation::ActivationRecord) -> Vec<String> {
    let mut ids = Vec::new();
    if !record.links.is_empty() {
        ids.push("path-shim".to_owned());
    }
    for service in &record.services {
        ids.push(format!("systemd-user:{}", service.service));
    }
    ids
}

fn select_target<'a>(
    report: &'a targets::TargetReport,
    requested_target: Option<&str>,
) -> Result<&'a targets::ActivationTarget> {
    if let Some(requested_target) = requested_target {
        return report
            .targets
            .iter()
            .find(|target| target.id == requested_target)
            .with_context(|| format!("activation target not found: {requested_target}"));
    }

    let supported = report
        .targets
        .iter()
        .filter(|target| target.supported)
        .collect::<Vec<_>>();

    match supported.as_slice() {
        [target] => Ok(*target),
        [] => anyhow::bail!("no supported activation target found; run forkpkg targets first"),
        _ => {
            let ids = supported
                .iter()
                .map(|target| target.id.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            anyhow::bail!("multiple activation targets found ({ids}); rerun with --target <id>")
        }
    }
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
    format!(
        "{}/{}",
        workspace::sanitize_workspace_name(package),
        workspace::sanitize_workspace_name(label)
    )
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
