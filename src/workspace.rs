use std::env;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};

pub struct Workspace {
    pub root: PathBuf,
    pub source: PathBuf,
    pub metadata: PathBuf,
}

pub const DEFAULT_LABEL: &str = "default";

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

pub fn forks_dir() -> Result<PathBuf> {
    Ok(data_home()?.join("forkpkg").join("forks"))
}

pub fn state_home() -> Result<PathBuf> {
    Ok(match env::var_os("XDG_STATE_HOME").map(PathBuf::from) {
        Some(path) if path.is_absolute() => path,
        _ => home_dir()?.join(".local").join("state"),
    })
}

pub fn managed_workspace(package: &str, label: &str) -> Result<PathBuf> {
    Ok(forks_dir()?
        .join(sanitize_workspace_name(package))
        .join(sanitize_workspace_name(label)))
}

pub fn legacy_workspace(package: &str) -> Result<PathBuf> {
    Ok(forks_dir()?.join(sanitize_workspace_name(package)))
}

pub fn legacy_workspace_exists(package: &str) -> Result<bool> {
    Ok(legacy_workspace(package)?.join("forkpkg.toml").is_file())
}

pub fn create_managed(package: &str, label: &str) -> Result<Workspace> {
    let root = managed_workspace(package, label)?;
    if root.exists() {
        bail!("fork workspace already exists: {}", root.display());
    }

    fs::create_dir_all(root.join("source"))
        .with_context(|| format!("failed to create {}", root.display()))?;
    Ok(Workspace::new(root))
}

pub fn list_managed() -> Result<Vec<Workspace>> {
    let forks = forks_dir()?;
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
            continue;
        }

        if !path.is_dir() {
            continue;
        }

        for label_entry in
            fs::read_dir(&path).with_context(|| format!("failed to read {}", path.display()))?
        {
            let label_entry = label_entry
                .with_context(|| format!("failed to read entry in {}", path.display()))?;
            let label_path = label_entry.path();
            if label_path.join("forkpkg.toml").is_file() {
                workspaces.push(Workspace::new(label_path));
            }
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

pub fn resolve_labeled(reference: Option<PathBuf>, label: Option<&str>) -> Result<Workspace> {
    let Some(reference) = reference else {
        if label.is_some() {
            bail!("--label requires a managed fork name");
        }
        return find(None);
    };

    if reference.exists() {
        if label.is_some() {
            bail!("--label cannot be used with a filesystem path");
        }
        return find(Some(reference));
    }

    if let Some((package, inline_label)) = managed_reference(&reference) {
        if label.is_some() && inline_label.is_some() {
            bail!("use either --label or package/label, not both");
        }

        let label = label.or(inline_label.as_deref()).unwrap_or(DEFAULT_LABEL);
        return resolve_managed_reference(&package, Some(label));
    }

    bail!("path does not exist: {}", reference.display());
}

fn resolve_managed_reference(package: &str, label: Option<&str>) -> Result<Workspace> {
    if let Some(label) = label {
        if label == DEFAULT_LABEL {
            let legacy = legacy_workspace(package)?;
            if legacy.join("forkpkg.toml").is_file() {
                return Ok(Workspace::new(legacy));
            }
        }

        let root = managed_workspace(package, label)?;
        if root.join("forkpkg.toml").is_file() {
            return Ok(Workspace::new(root));
        }
        bail!("no managed fork named {package}/{label} found");
    }

    let legacy = legacy_workspace(package)?;
    if legacy.join("forkpkg.toml").is_file() {
        return Ok(Workspace::new(legacy));
    }

    let default = managed_workspace(package, DEFAULT_LABEL)?;
    if default.join("forkpkg.toml").is_file() {
        return Ok(Workspace::new(default));
    }

    let workspaces = list_package(package)?;
    match workspaces.as_slice() {
        [workspace] => Ok(Workspace::new(workspace.root.clone())),
        [] => bail!(
            "no managed fork named {package:?} found under {}",
            forks_dir()?.display()
        ),
        _ => bail!("managed fork name {package:?} is ambiguous; use {package}/<label>"),
    }
}

fn managed_reference(reference: &Path) -> Option<(String, Option<String>)> {
    let mut parts = Vec::new();
    for component in reference.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_string_lossy().into_owned()),
            Component::CurDir
            | Component::ParentDir
            | Component::RootDir
            | Component::Prefix(_) => return None,
        }
    }

    match parts.as_slice() {
        [package] => Some((package.clone(), None)),
        [package, label] => Some((package.clone(), Some(label.clone()))),
        _ => None,
    }
}

pub fn list_package(package: &str) -> Result<Vec<Workspace>> {
    let mut workspaces = Vec::new();

    let legacy = legacy_workspace(package)?;
    if legacy.join("forkpkg.toml").is_file() {
        workspaces.push(Workspace::new(legacy));
    }

    let package_dir = forks_dir()?.join(sanitize_workspace_name(package));
    if package_dir.is_dir() && !package_dir.join("forkpkg.toml").is_file() {
        for entry in fs::read_dir(&package_dir)
            .with_context(|| format!("failed to read {}", package_dir.display()))?
        {
            let entry = entry
                .with_context(|| format!("failed to read entry in {}", package_dir.display()))?;
            let path = entry.path();
            if path.join("forkpkg.toml").is_file() {
                workspaces.push(Workspace::new(path));
            }
        }
    }

    workspaces.sort_by(|left, right| left.root.cmp(&right.root));
    Ok(workspaces)
}

pub fn migrate_legacy_to_default(package: &str) -> Result<Option<Workspace>> {
    let legacy = legacy_workspace(package)?;
    if !legacy.join("forkpkg.toml").is_file() {
        return Ok(None);
    }

    let default = managed_workspace(package, DEFAULT_LABEL)?;
    if default.exists() {
        bail!(
            "cannot migrate legacy fork because default fork already exists: {}",
            default.display()
        );
    }

    let forks = forks_dir()?;
    let temporary = forks.join(format!(
        ".{}-migrating-{}",
        sanitize_workspace_name(package),
        std::process::id()
    ));
    if temporary.exists() {
        bail!(
            "temporary migration path already exists: {}",
            temporary.display()
        );
    }

    fs::rename(&legacy, &temporary).with_context(|| {
        format!(
            "failed to move legacy fork {} to {}",
            legacy.display(),
            temporary.display()
        )
    })?;
    fs::create_dir_all(&legacy)
        .with_context(|| format!("failed to create {}", legacy.display()))?;
    fs::rename(&temporary, &default).with_context(|| {
        format!(
            "failed to move legacy fork {} to {}",
            temporary.display(),
            default.display()
        )
    })?;

    Ok(Some(Workspace::new(default)))
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

fn data_home() -> Result<PathBuf> {
    Ok(match env::var_os("XDG_DATA_HOME").map(PathBuf::from) {
        Some(path) if path.is_absolute() => path,
        _ => home_dir()?.join(".local").join("share"),
    })
}

fn home_dir() -> Result<PathBuf> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or_else(|| anyhow!("HOME is not set to an absolute path"))
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

pub fn stable_name(display_name: &str, identity: &str) -> String {
    let display = sanitize_workspace_name(display_name);
    let digest = blake3::hash(identity.as_bytes()).to_hex();
    format!("{display}-{}", &digest[..12])
}

#[cfg(test)]
mod tests {
    use super::{sanitize_workspace_name, stable_name};

    #[test]
    fn sanitizes_workspace_names() {
        assert_eq!(sanitize_workspace_name("ripgrep"), "ripgrep");
        assert_eq!(
            sanitize_workspace_name("xdg desktop portal/gnome"),
            "xdg-desktop-portal-gnome"
        );
        assert_eq!(sanitize_workspace_name("..."), "fork");
    }

    #[test]
    fn stable_names_keep_display_text_and_distinguish_identity() {
        let first = stable_name("ripgrep", "nixpkgs#ripgrep:x86_64-linux");
        let second = stable_name("ripgrep", "nixpkgs#ripgrep:aarch64-linux");

        assert!(first.starts_with("ripgrep-"));
        assert_ne!(first, second);
    }
}
