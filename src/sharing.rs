use std::fs::{self, File, OpenOptions};
use std::io::Cursor;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::git;
use crate::metadata::Metadata;
use crate::workspace::Workspace;

const SHARE_METADATA: &str = "forkpkg-share.toml";
const BUNDLE_ENTRY: &str = "commits.bundle";
const PATCH_ENTRY: &str = "commits.patch";

#[derive(Debug)]
pub struct ExportSummary {
    pub artifact: PathBuf,
    pub base_commit: String,
    pub head_commit: String,
    pub commit_count: u64,
}

#[derive(Debug)]
pub struct ImportSummary {
    pub artifact: PathBuf,
    pub method: &'static str,
    pub base_commit: String,
    pub head_commit: String,
    pub commit_count: u64,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct ShareArtifact {
    format: u32,
    kind: String,
    package: SharePackage,
    base: ShareBase,
    changes: ShareChanges,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct SharePackage {
    installable: String,
    flake_ref: String,
    attribute: String,
    system: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pname: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<String>,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct ShareBase {
    #[serde(skip_serializing_if = "Option::is_none")]
    nixpkgs_revision: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    nixpkgs_last_modified: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    nixpkgs_locked_nar_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    nixpkgs_resolved_url: Option<String>,
    derivation: String,
    output: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_revision: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_ca: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    post_patch_source_hash: Option<String>,
    git_commit: String,
    git_tree: String,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct ShareChanges {
    base_commit: String,
    base_tree: String,
    head_commit: String,
    commit_count: u64,
    bundle_path: String,
    patch_path: String,
}

pub fn export_changes(
    workspace: &Workspace,
    metadata: &Metadata,
    output: &Path,
) -> Result<ExportSummary> {
    if output.exists() {
        bail!(
            "refusing to overwrite existing artifact: {}",
            output.display()
        );
    }

    let repo = git::repo_state(&workspace.source, &metadata.base.git_commit)?;
    if !repo.base_commit_present {
        bail!(
            "base commit is missing from source repo: {}",
            metadata.base.git_commit
        );
    }
    if repo.dirty {
        bail!("source has uncommitted changes; commit or stash them before exporting");
    }

    let commit_count = repo
        .commits_on_top
        .ok_or_else(|| anyhow!("could not count commits on top of the base commit"))?;
    if commit_count == 0 {
        bail!("fork has no commits on top of the base commit to export");
    }

    let base_tree = git::tree_hash(&workspace.source, &metadata.base.git_commit)?;
    let temp = tempfile::tempdir().context("failed to create temporary export directory")?;
    let bundle_path = temp.path().join(BUNDLE_ENTRY);
    let patch_path = temp.path().join(PATCH_ENTRY);

    git::create_bundle(&workspace.source, &metadata.base.git_commit, &bundle_path)?;
    git::format_patch(&workspace.source, &metadata.base.git_commit, &patch_path)?;

    let share = ShareArtifact {
        format: 1,
        kind: "forkpkg-share".to_owned(),
        package: SharePackage {
            installable: metadata.package.installable.clone(),
            flake_ref: metadata.package.flake_ref.clone(),
            attribute: metadata.package.attribute.clone(),
            system: metadata.package.system.clone(),
            name: metadata.package.name.clone(),
            pname: metadata.package.pname.clone(),
            version: metadata.package.version.clone(),
        },
        base: ShareBase {
            nixpkgs_revision: metadata.base.nixpkgs_revision.clone(),
            nixpkgs_last_modified: metadata.base.nixpkgs_last_modified,
            nixpkgs_locked_nar_hash: metadata.base.nixpkgs_locked_nar_hash.clone(),
            nixpkgs_resolved_url: metadata.base.nixpkgs_resolved_url.clone(),
            derivation: metadata.base.derivation.clone(),
            output: metadata.base.output.clone(),
            source: metadata.base.source.clone(),
            source_revision: metadata.base.source_revision.clone(),
            source_hash: metadata.base.source_hash.clone(),
            source_ca: metadata.base.source_ca.clone(),
            post_patch_source_hash: metadata.base.post_patch_source_hash.clone(),
            git_commit: metadata.base.git_commit.clone(),
            git_tree: base_tree.clone(),
        },
        changes: ShareChanges {
            base_commit: metadata.base.git_commit.clone(),
            base_tree,
            head_commit: repo.head_commit.clone(),
            commit_count,
            bundle_path: BUNDLE_ENTRY.to_owned(),
            patch_path: PATCH_ENTRY.to_owned(),
        },
    };

    write_artifact(output, &share, &bundle_path, &patch_path)?;

    Ok(ExportSummary {
        artifact: output.to_path_buf(),
        base_commit: metadata.base.git_commit.clone(),
        head_commit: repo.head_commit,
        commit_count,
    })
}

pub fn import_changes(
    workspace: &Workspace,
    metadata: &Metadata,
    artifact: &Path,
) -> Result<ImportSummary> {
    let extracted = read_artifact(artifact)?;
    let share = extracted.share;
    validate_share_metadata(&share, metadata)?;

    let repo = git::repo_state(&workspace.source, &metadata.base.git_commit)?;
    if !repo.base_commit_present {
        bail!(
            "target fork is missing its recorded base commit: {}",
            metadata.base.git_commit
        );
    }
    if repo.dirty {
        bail!("target source has uncommitted changes; commit or stash them before importing");
    }

    let target_base_tree = git::tree_hash(&workspace.source, &metadata.base.git_commit)?;
    if target_base_tree != share.changes.base_tree {
        bail!(
            "base tree mismatch; artifact expects {}, target fork has {}",
            share.changes.base_tree,
            target_base_tree
        );
    }

    let method = if metadata.base.git_commit == share.changes.base_commit {
        import_from_bundle(&workspace.source, &share, &extracted.bundle)?
    } else {
        git::apply_patch_mailbox(&workspace.source, &extracted.patch)?;
        "git-am"
    };

    let head_commit = git::repo_state(&workspace.source, &metadata.base.git_commit)?.head_commit;

    Ok(ImportSummary {
        artifact: artifact.to_path_buf(),
        method,
        base_commit: metadata.base.git_commit.clone(),
        head_commit,
        commit_count: share.changes.commit_count,
    })
}

fn import_from_bundle(repo: &Path, share: &ShareArtifact, bundle: &Path) -> Result<&'static str> {
    git::bundle_verify(repo, bundle)?;

    let target_ref = format!(
        "refs/forkpkg/imports/{}",
        short_hash(&share.changes.head_commit)
    );
    git::fetch_bundle_ref(repo, bundle, &target_ref)?;

    if git::is_ancestor(repo, &target_ref, "HEAD")? {
        return Ok("already-present");
    }

    if git::is_ancestor(repo, "HEAD", &target_ref)? {
        git::merge_ff_only(repo, &target_ref)?;
        return Ok("fast-forward");
    }

    git::cherry_pick_range(repo, &share.changes.base_commit, &target_ref)?;
    Ok("cherry-pick")
}

fn validate_share_metadata(share: &ShareArtifact, metadata: &Metadata) -> Result<()> {
    if share.format != 1 || share.kind != "forkpkg-share" {
        bail!("unsupported forkpkg share artifact");
    }

    let mut mismatches = Vec::new();
    compare_field(
        &mut mismatches,
        "package.flake_ref",
        &share.package.flake_ref,
        &metadata.package.flake_ref,
    );
    compare_field(
        &mut mismatches,
        "package.attribute",
        &share.package.attribute,
        &metadata.package.attribute,
    );
    compare_field(
        &mut mismatches,
        "package.system",
        &share.package.system,
        &metadata.package.system,
    );
    compare_field(
        &mut mismatches,
        "base.derivation",
        &share.base.derivation,
        &metadata.base.derivation,
    );
    compare_field(
        &mut mismatches,
        "base.output",
        &share.base.output,
        &metadata.base.output,
    );
    compare_option(
        &mut mismatches,
        "base.nixpkgs_revision",
        share.base.nixpkgs_revision.as_deref(),
        metadata.base.nixpkgs_revision.as_deref(),
    );
    compare_option(
        &mut mismatches,
        "base.nixpkgs_locked_nar_hash",
        share.base.nixpkgs_locked_nar_hash.as_deref(),
        metadata.base.nixpkgs_locked_nar_hash.as_deref(),
    );
    compare_option(
        &mut mismatches,
        "base.nixpkgs_resolved_url",
        share.base.nixpkgs_resolved_url.as_deref(),
        metadata.base.nixpkgs_resolved_url.as_deref(),
    );
    compare_option(
        &mut mismatches,
        "base.source",
        share.base.source.as_deref(),
        metadata.base.source.as_deref(),
    );
    compare_option(
        &mut mismatches,
        "base.source_revision",
        share.base.source_revision.as_deref(),
        metadata.base.source_revision.as_deref(),
    );
    compare_option(
        &mut mismatches,
        "base.source_hash",
        share.base.source_hash.as_deref(),
        metadata.base.source_hash.as_deref(),
    );
    compare_option(
        &mut mismatches,
        "base.post_patch_source_hash",
        share.base.post_patch_source_hash.as_deref(),
        metadata.base.post_patch_source_hash.as_deref(),
    );

    if !mismatches.is_empty() {
        bail!(
            "artifact does not match this fork:\n{}",
            mismatches
                .into_iter()
                .map(|mismatch| format!("  {mismatch}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }

    Ok(())
}

fn compare_field(mismatches: &mut Vec<String>, label: &str, left: &str, right: &str) {
    if left != right {
        mismatches.push(format!("{label}: artifact={left:?} target={right:?}"));
    }
}

fn compare_option(
    mismatches: &mut Vec<String>,
    label: &str,
    left: Option<&str>,
    right: Option<&str>,
) {
    if left != right {
        mismatches.push(format!("{label}: artifact={left:?} target={right:?}"));
    }
}

fn write_artifact(
    output: &Path,
    share: &ShareArtifact,
    bundle_path: &Path,
    patch_path: &Path,
) -> Result<()> {
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output)
        .with_context(|| format!("failed to create {}", output.display()))?;
    let mut builder = tar::Builder::new(file);

    let metadata = toml::to_string_pretty(share)
        .context("failed to serialize forkpkg share metadata")?
        .into_bytes();
    let mut header = tar::Header::new_gnu();
    header.set_size(metadata.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    builder
        .append_data(
            &mut header,
            SHARE_METADATA,
            &mut Cursor::new(metadata.as_slice()),
        )
        .context("failed to write artifact metadata")?;
    builder
        .append_path_with_name(bundle_path, BUNDLE_ENTRY)
        .context("failed to write Git bundle to artifact")?;
    builder
        .append_path_with_name(patch_path, PATCH_ENTRY)
        .context("failed to write Git patch stream to artifact")?;
    builder.finish().context("failed to finish artifact")?;

    Ok(())
}

struct ExtractedArtifact {
    _temp: tempfile::TempDir,
    share: ShareArtifact,
    bundle: PathBuf,
    patch: PathBuf,
}

fn read_artifact(path: &Path) -> Result<ExtractedArtifact> {
    let file = File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let temp = tempfile::tempdir().context("failed to create temporary import directory")?;
    let mut archive = tar::Archive::new(file);

    for entry in archive
        .entries()
        .context("failed to read artifact entries")?
    {
        let mut entry = entry.context("failed to read artifact entry")?;
        if !entry.header().entry_type().is_file() {
            bail!("artifact contains a non-file entry");
        }

        let entry_path = entry.path().context("artifact entry path is invalid")?;
        let name = match entry_path.as_ref() {
            path if path == Path::new(SHARE_METADATA) => SHARE_METADATA,
            path if path == Path::new(BUNDLE_ENTRY) => BUNDLE_ENTRY,
            path if path == Path::new(PATCH_ENTRY) => PATCH_ENTRY,
            other => bail!("artifact contains unexpected entry: {}", other.display()),
        };

        entry
            .unpack(temp.path().join(name))
            .with_context(|| format!("failed to extract {name}"))?;
    }

    let metadata_path = temp.path().join(SHARE_METADATA);
    let bundle = temp.path().join(BUNDLE_ENTRY);
    let patch = temp.path().join(PATCH_ENTRY);

    if !metadata_path.is_file() || !bundle.is_file() || !patch.is_file() {
        bail!("artifact is missing required metadata, bundle, or patch payload");
    }

    let text = fs::read_to_string(&metadata_path)
        .with_context(|| format!("failed to read {}", metadata_path.display()))?;
    let share = toml::from_str(&text).context("failed to parse forkpkg share metadata")?;

    Ok(ExtractedArtifact {
        _temp: temp,
        share,
        bundle,
        patch,
    })
}

fn short_hash(hash: &str) -> &str {
    hash.get(..12).unwrap_or(hash)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{
        BUNDLE_ENTRY, PATCH_ENTRY, SHARE_METADATA, ShareArtifact, ShareBase, ShareChanges,
        SharePackage, read_artifact, write_artifact,
    };

    #[test]
    fn share_artifact_round_trips_metadata_and_payloads() {
        let temp = tempfile::tempdir().unwrap();
        let bundle = temp.path().join(BUNDLE_ENTRY);
        let patch = temp.path().join(PATCH_ENTRY);
        let output = temp.path().join("change.forkpkg");
        fs::write(&bundle, b"bundle").unwrap();
        fs::write(&patch, b"patch").unwrap();

        let share = sample_share();
        write_artifact(&output, &share, &bundle, &patch).unwrap();

        let extracted = read_artifact(&output).unwrap();
        assert_eq!(extracted.share, share);
        assert_eq!(fs::read(extracted.bundle).unwrap(), b"bundle");
        assert_eq!(fs::read(extracted.patch).unwrap(), b"patch");
    }

    #[test]
    fn share_artifact_rejects_unexpected_entries() {
        let temp = tempfile::tempdir().unwrap();
        let output = temp.path().join("bad.forkpkg");
        let file = fs::File::create(&output).unwrap();
        let mut builder = tar::Builder::new(file);
        let mut header = tar::Header::new_gnu();
        header.set_size(3);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, "surprise", &mut std::io::Cursor::new(b"bad"))
            .unwrap();
        builder.finish().unwrap();

        assert!(read_artifact(&output).is_err());
    }

    fn sample_share() -> ShareArtifact {
        ShareArtifact {
            format: 1,
            kind: "forkpkg-share".to_owned(),
            package: SharePackage {
                installable: "nixpkgs#hello".to_owned(),
                flake_ref: "nixpkgs".to_owned(),
                attribute: "hello".to_owned(),
                system: "x86_64-linux".to_owned(),
                name: Some("hello-2.12.2".to_owned()),
                pname: Some("hello".to_owned()),
                version: Some("2.12.2".to_owned()),
            },
            base: ShareBase {
                nixpkgs_revision: Some("abc123".to_owned()),
                nixpkgs_last_modified: Some(1),
                nixpkgs_locked_nar_hash: Some("sha256-test".to_owned()),
                nixpkgs_resolved_url: Some("github:NixOS/nixpkgs/abc123".to_owned()),
                derivation: "/nix/store/example.drv".to_owned(),
                output: "/nix/store/example-hello".to_owned(),
                source: Some("/nix/store/source".to_owned()),
                source_revision: Some("def456".to_owned()),
                source_hash: Some("sha256-source".to_owned()),
                source_ca: Some("fixed:r:sha256:source".to_owned()),
                post_patch_source_hash: Some("sha256-post-patch".to_owned()),
                git_commit: "0123456789abcdef".to_owned(),
                git_tree: "abcdef0123456789".to_owned(),
            },
            changes: ShareChanges {
                base_commit: "0123456789abcdef".to_owned(),
                base_tree: "abcdef0123456789".to_owned(),
                head_commit: "fedcba9876543210".to_owned(),
                commit_count: 2,
                bundle_path: BUNDLE_ENTRY.to_owned(),
                patch_path: PATCH_ENTRY.to_owned(),
            },
        }
    }

    #[test]
    fn short_hash_handles_short_inputs() {
        assert_eq!(super::short_hash("abcdef"), "abcdef");
        assert_eq!(super::short_hash("abcdefghijklmnop"), "abcdefghijkl");
        assert_eq!(SHARE_METADATA, "forkpkg-share.toml");
    }
}
