use std::env;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};

pub struct Workspace {
    pub root: PathBuf,
    pub source: PathBuf,
    pub metadata: PathBuf,
}

impl Workspace {
    pub fn new(root: PathBuf) -> Self {
        let source = root.join("source");
        let metadata = root.join("forkpkg.toml");
        Self {
            root,
            source,
            metadata,
        }
    }
}

pub fn forks_dir() -> PathBuf {
    data_home().join("forkpkg").join("forks")
}

pub fn state_home() -> PathBuf {
    match env::var_os("XDG_STATE_HOME").map(PathBuf::from) {
        Some(path) if path.is_absolute() => path,
        _ => home_dir().join(".local").join("state"),
    }
}

pub fn managed_workspace(name: &str) -> PathBuf {
    forks_dir().join(sanitize_workspace_name(name))
}

pub fn create_managed(name: &str) -> Result<Workspace> {
    let root = managed_workspace(name);
    if root.exists() {
        bail!("fork workspace already exists: {}", root.display());
    }

    fs::create_dir_all(root.join("source"))
        .with_context(|| format!("failed to create {}", root.display()))?;
    Ok(Workspace::new(root))
}

pub fn list_managed() -> Result<Vec<Workspace>> {
    let forks = forks_dir();
    if !forks.exists() {
        return Ok(Vec::new());
    }

    let mut workspaces = Vec::new();
    for entry in fs::read_dir(&forks)
        .with_context(|| format!("failed to read forks directory {}", forks.display()))?
    {
        let entry =
            entry.with_context(|| format!("failed to read entry in {}", forks.display()))?;
        let path = entry.path();
        if path.join("forkpkg.toml").is_file() {
            workspaces.push(Workspace::new(path));
        }
    }

    workspaces.sort_by(|left, right| left.root.cmp(&right.root));
    Ok(workspaces)
}

pub fn find(start: Option<PathBuf>) -> Result<Workspace> {
    let start = match start {
        Some(path) => path,
        None => env::current_dir().context("failed to determine current directory")?,
    };

    let mut current = if start.is_file() {
        start
            .parent()
            .ok_or_else(|| anyhow!("path has no parent: {}", start.display()))?
            .to_path_buf()
    } else {
        start
    };

    current = current
        .canonicalize()
        .with_context(|| format!("failed to resolve {}", current.display()))?;

    loop {
        let metadata = current.join("forkpkg.toml");
        if metadata.is_file() {
            return Ok(Workspace::new(current));
        }

        if !current.pop() {
            bail!("no forkpkg.toml found from the requested path upward");
        }
    }
}

pub fn copy_tree(source: &Path, destination: &Path) -> Result<()> {
    if !source.is_dir() {
        bail!("source is not a directory: {}", source.display());
    }

    fs::create_dir_all(destination)
        .with_context(|| format!("failed to create {}", destination.display()))?;

    for entry in fs::read_dir(source)
        .with_context(|| format!("failed to read directory {}", source.display()))?
    {
        let entry =
            entry.with_context(|| format!("failed to read entry in {}", source.display()))?;
        copy_entry(&entry.path(), &destination.join(entry.file_name()))?;
    }

    make_writable(destination, true)?;
    Ok(())
}

fn copy_entry(source: &Path, destination: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(source)
        .with_context(|| format!("failed to inspect {}", source.display()))?;
    let file_type = metadata.file_type();

    if file_type.is_symlink() {
        let target = fs::read_link(source)
            .with_context(|| format!("failed to read symlink {}", source.display()))?;
        symlink(&target, destination)
            .with_context(|| format!("failed to create symlink {}", destination.display()))?;
        return Ok(());
    }

    if file_type.is_dir() {
        fs::create_dir(destination)
            .with_context(|| format!("failed to create directory {}", destination.display()))?;
        for entry in fs::read_dir(source)
            .with_context(|| format!("failed to read directory {}", source.display()))?
        {
            let entry =
                entry.with_context(|| format!("failed to read entry in {}", source.display()))?;
            copy_entry(&entry.path(), &destination.join(entry.file_name()))?;
        }
        make_writable(destination, true)?;
        return Ok(());
    }

    if file_type.is_file() {
        fs::copy(source, destination).with_context(|| {
            format!(
                "failed to copy {} to {}",
                source.display(),
                destination.display()
            )
        })?;
        let mode = metadata.permissions().mode();
        fs::set_permissions(destination, fs::Permissions::from_mode(mode))
            .with_context(|| format!("failed to set permissions on {}", destination.display()))?;
        make_writable(destination, false)?;
        return Ok(());
    }

    bail!("unsupported source entry type: {}", source.display());
}

fn make_writable(path: &Path, is_dir: bool) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect {}", path.display()))?;
    let mut mode = metadata.permissions().mode();
    mode |= if is_dir { 0o700 } else { 0o600 };
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .with_context(|| format!("failed to make {} writable", path.display()))
}

fn data_home() -> PathBuf {
    match env::var_os("XDG_DATA_HOME").map(PathBuf::from) {
        Some(path) if path.is_absolute() => path,
        _ => home_dir().join(".local").join("share"),
    }
}

fn home_dir() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn sanitize_workspace_name(name: &str) -> String {
    let mut out = String::new();
    let mut previous_dash = false;

    for ch in name.chars() {
        let mapped = if ch.is_ascii_alphanumeric() || ch == '.' || ch == '_' || ch == '-' {
            ch
        } else {
            '-'
        };

        if mapped == '-' {
            if previous_dash {
                continue;
            }
            previous_dash = true;
        } else {
            previous_dash = false;
        }

        out.push(mapped);
    }

    let out = out.trim_matches(['.', '-', '_']).to_owned();
    if out.is_empty() {
        "fork".to_owned()
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::sanitize_workspace_name;

    #[test]
    fn sanitizes_workspace_names() {
        assert_eq!(sanitize_workspace_name("ripgrep"), "ripgrep");
        assert_eq!(
            sanitize_workspace_name("xdg desktop portal/gnome"),
            "xdg-desktop-portal-gnome"
        );
        assert_eq!(sanitize_workspace_name("..."), "fork");
    }
}
