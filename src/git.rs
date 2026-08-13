use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, anyhow};

#[derive(Debug, Clone)]
pub struct RepoState {
    pub base_commit: String,
    pub base_commit_present: bool,
    pub head_commit: String,
    pub commits_on_top: Option<u64>,
    pub dirty: bool,
}

pub fn init_base_commit(source: &Path, message: &str) -> Result<String> {
    run_git(source, ["init"])?;
    run_git(source, ["branch", "-M", "main"])?;
    run_git(source, ["add", "-A"])?;

    let status = git_output(source, ["status", "--porcelain"])?;
    if status.trim().is_empty() {
        return Err(anyhow!(
            "source tree is empty; refusing to create an empty base commit"
        ));
    }

    let mut commit = Command::new("git");
    commit
        .arg("-C")
        .arg(source)
        .args(["-c", "user.name=forkpkg"])
        .args(["-c", "user.email=forkpkg@example.invalid"])
        .args(["-c", "commit.gpgsign=false"])
        .args(["commit", "--no-gpg-sign", "-m", message]);
    run_command(&mut commit, "git commit")?;

    Ok(git_output(source, ["rev-parse", "HEAD"])?.trim().to_owned())
}

pub fn repo_state(source: &Path, base_commit: &str) -> Result<RepoState> {
    let head_commit = git_output(source, ["rev-parse", "HEAD"])?.trim().to_owned();
    let dirty = !git_output(source, ["status", "--porcelain"])?
        .trim()
        .is_empty();

    let base_commit_present = git_success(
        source,
        &[
            "rev-parse",
            "--verify",
            &format!("{base_commit}^{{commit}}"),
        ],
    )?;
    let commits_on_top = if base_commit_present {
        let range = format!("{base_commit}..HEAD");
        let count = git_output_args(source, &["rev-list", "--count", &range])?;
        Some(
            count
                .trim()
                .parse()
                .with_context(|| format!("git rev-list returned invalid count: {count:?}"))?,
        )
    } else {
        None
    };

    Ok(RepoState {
        base_commit: base_commit.to_owned(),
        base_commit_present,
        head_commit,
        commits_on_top,
        dirty,
    })
}

pub fn tree_hash(repo: &Path, rev: &str) -> Result<String> {
    let tree = format!("{rev}^{{tree}}");
    Ok(git_output_args(repo, &["rev-parse", &tree])?
        .trim()
        .to_owned())
}

pub fn is_ancestor(repo: &Path, ancestor: &str, descendant: &str) -> Result<bool> {
    git_success(repo, &["merge-base", "--is-ancestor", ancestor, descendant])
}

pub fn create_bundle(repo: &Path, base_commit: &str, output: &Path) -> Result<()> {
    let excluded_base = format!("^{base_commit}");
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(repo)
        .args(["bundle", "create"])
        .arg(output)
        .args(["HEAD", &excluded_base]);
    run_command(&mut command, "git bundle create")
}

pub fn format_patch(repo: &Path, base_commit: &str, output: &Path) -> Result<()> {
    let range = format!("{base_commit}..HEAD");
    let output_data = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["format-patch", "--stdout", &range])
        .output()
        .context("failed to execute git format-patch")?;

    if !output_data.status.success() {
        return Err(anyhow!(
            "git format-patch failed with status {}\n{}",
            output_data.status,
            String::from_utf8_lossy(&output_data.stderr)
        ));
    }

    std::fs::write(output, output_data.stdout)
        .with_context(|| format!("failed to write {}", output.display()))
}

pub fn bundle_verify(repo: &Path, bundle: &Path) -> Result<()> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(repo)
        .args(["bundle", "verify"])
        .arg(bundle);
    run_command(&mut command, "git bundle verify")
}

pub fn fetch_bundle_ref(repo: &Path, bundle: &Path, target_ref: &str) -> Result<()> {
    let refspec = format!("HEAD:{target_ref}");
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(repo)
        .arg("fetch")
        .arg(bundle)
        .arg(refspec);
    run_command(&mut command, "git fetch")
}

pub fn merge_ff_only(repo: &Path, rev: &str) -> Result<()> {
    run_git_args(repo, &["merge", "--ff-only", rev])
}

pub fn cherry_pick_range(repo: &Path, base_commit: &str, head_ref: &str) -> Result<()> {
    let range = format!("{base_commit}..{head_ref}");
    run_git_args(repo, &["cherry-pick", &range])
}

pub fn apply_patch_mailbox(repo: &Path, patch: &Path) -> Result<()> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(repo)
        .args(["am", "--3way"])
        .arg(patch);
    run_command(&mut command, "git am")
}

fn run_git<const N: usize>(repo: &Path, args: [&str; N]) -> Result<()> {
    let mut command = Command::new("git");
    command.arg("-C").arg(repo).args(args);
    run_command(&mut command, "git")
}

fn run_git_args(repo: &Path, args: &[&str]) -> Result<()> {
    let mut command = Command::new("git");
    command.arg("-C").arg(repo).args(args);
    run_command(&mut command, "git")
}

fn git_output<const N: usize>(repo: &Path, args: [&str; N]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .context("failed to execute git")?;

    if !output.status.success() {
        return Err(anyhow!(
            "git failed with status {}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    String::from_utf8(output.stdout).context("git output was not valid UTF-8")
}

fn git_output_args(repo: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .context("failed to execute git")?;

    if !output.status.success() {
        return Err(anyhow!(
            "git failed with status {}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    String::from_utf8(output.stdout).context("git output was not valid UTF-8")
}

fn git_success(repo: &Path, args: &[&str]) -> Result<bool> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .context("failed to execute git")?;
    Ok(output.status.success())
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
