mod activation;
mod cli;
mod git;
mod metadata;
mod nix;
mod workspace;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use serde::Serialize;

use crate::activation::{ActivationRecordEntry, PreviousLink};
use crate::cli::{Cli, Command};
use crate::metadata::{BaseMetadata, BuildMetadata, Metadata, PackageMetadata};

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Fork { installable } => fork(&installable),
        Command::List { json } => list(json),
        Command::Build { path } => build(path),
        Command::Info { path } => info(path),
        Command::Enable { path, dry_run } => enable(path, dry_run),
        Command::Disable { path, dry_run } => disable(path, dry_run),
        Command::DisableAll { dry_run } => disable_all(dry_run),
        Command::Doctor => doctor(),
        Command::Status { path } => status(path),
    }
}

fn fork(installable: &str) -> Result<()> {
    eprintln!("resolving {installable}");
    let resolved = nix::resolve_installable(installable)?;
    let workspace_name = workspace_name(&resolved);
    let workspace_path = workspace::managed_workspace(&workspace_name)?;
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
    let workspace = workspace::create_managed(&workspace_name)?;
    workspace::copy_tree(&post_patch_source, &workspace.source)?;

    let base_description = base_description(&resolved);
    eprintln!("initializing Git base commit");
    let base_commit = git::init_base_commit(
        &workspace.source,
        &format!("forkpkg base: {base_description}"),
    )?;

    let metadata = Metadata {
        format: 1,
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
            fork.package,
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

        forks.push(ForkListEntry {
            package,
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

fn info(path: Option<PathBuf>) -> Result<()> {
    let workspace = workspace::resolve(path)?;
    let metadata = Metadata::read(&workspace.metadata)?;

    print_optional("package", metadata.package.pname.as_deref());
    print_optional("name", metadata.package.name.as_deref());
    print_optional("version", metadata.package.version.as_deref());
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

fn enable(path: Option<PathBuf>, dry_run: bool) -> Result<()> {
    let workspace = workspace::resolve(path)?;
    let metadata = Metadata::read(&workspace.metadata)?;

    if dry_run {
        eprintln!("building fork to preview activation");
    } else {
        eprintln!("building fork before activation");
    }
    let output = nix::build_local_source(&metadata, &workspace.source)?;
    let record = if dry_run {
        activation::plan_path_shim(&metadata, &workspace, &output)?
    } else {
        activation::enable_path_shim(&metadata, &workspace, &output)?
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

fn disable(path: Option<PathBuf>, dry_run: bool) -> Result<()> {
    let workspace = workspace::resolve(path)?;
    let metadata = Metadata::read(&workspace.metadata)?;
    let record = if dry_run {
        activation::disable_plan(&metadata)?
    } else {
        activation::disable(&metadata)?
    };

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
                let package = activation::package_display_name(&metadata);
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
                let check = activation::check_record(record);
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

    match activation::status(&metadata)? {
        Some(record) => {
            let check = activation::check_record(record.clone());
            println!("fork: {package}");
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
        }
        None => {
            println!("fork: {package}");
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
}

fn workspace_name(resolved: &nix::ResolvedPackage) -> String {
    let display_name = resolved
        .package_pname
        .as_deref()
        .or_else(|| resolved.installable.attr_path.last().map(String::as_str))
        .or(resolved.package_name.as_deref())
        .unwrap_or("package")
        .to_owned();
    workspace::stable_name(&display_name, &resolved_identity(resolved))
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

fn resolved_identity(resolved: &nix::ResolvedPackage) -> String {
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
        resolved.installable.original,
        resolved.installable.flake_ref,
        resolved.installable.attribute,
        resolved.system,
        resolved.flake.revision.as_deref().unwrap_or(""),
        resolved.flake.locked_nar_hash.as_deref().unwrap_or(""),
        resolved.flake.resolved_url.as_deref().unwrap_or(""),
        resolved.flake.path.as_deref().unwrap_or(""),
    )
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
