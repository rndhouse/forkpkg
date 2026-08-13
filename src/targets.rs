use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct TargetReport {
    pub output: PathBuf,
    pub targets: Vec<ActivationTarget>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ActivationTarget {
    pub id: String,
    pub kind: String,
    pub confidence: String,
    pub supported: bool,
    pub active: bool,
    pub evidence: Vec<String>,
    pub details: TargetDetails,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TargetDetails {
    PathShim {
        executables: Vec<PathBuf>,
        path_matches: Vec<PathMatch>,
    },
    SystemdUserService {
        service: String,
        service_file: PathBuf,
        exec_start: String,
        executable: Option<PathBuf>,
        dbus_names: Vec<String>,
    },
    DbusService {
        name: String,
        service_file: PathBuf,
        exec: Option<String>,
        systemd_service: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct PathMatch {
    pub name: String,
    pub path: PathBuf,
    pub resolved: Option<PathBuf>,
    pub points_to_output: bool,
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct SystemdUserServiceSpec {
    pub service: String,
    pub service_file: PathBuf,
    pub exec_start: String,
    pub executable: Option<PathBuf>,
}

#[derive(Debug, Clone)]
struct DbusServiceInfo {
    name: String,
    service_file: PathBuf,
    exec: Option<String>,
    systemd_service: Option<String>,
}

pub fn discover(output: &Path) -> Result<TargetReport> {
    let mut targets = Vec::new();
    let dbus_services = discover_dbus_services(output)?;
    let mut dbus_by_systemd = HashMap::<String, Vec<String>>::new();

    for dbus_service in &dbus_services {
        if let Some(systemd_service) = &dbus_service.systemd_service {
            dbus_by_systemd
                .entry(systemd_service.clone())
                .or_default()
                .push(dbus_service.name.clone());
        }
    }

    if let Some(target) = discover_path_shim(output)? {
        targets.push(target);
    }

    let systemd_targets = discover_systemd_user_services(output, &dbus_by_systemd)?;
    let discovered_systemd_services = systemd_targets
        .iter()
        .filter_map(|target| match &target.details {
            TargetDetails::SystemdUserService { service, .. } => Some(service.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    targets.extend(systemd_targets);

    for dbus_service in dbus_services {
        if dbus_service
            .systemd_service
            .as_ref()
            .is_some_and(|service| discovered_systemd_services.contains(service))
        {
            continue;
        }

        targets.push(dbus_target(dbus_service));
    }

    targets = dedupe_targets(targets);
    targets.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(TargetReport {
        output: output.to_path_buf(),
        targets,
    })
}

fn dedupe_targets(targets: Vec<ActivationTarget>) -> Vec<ActivationTarget> {
    let mut seen = HashSet::new();
    let mut deduped = Vec::new();

    for target in targets {
        if seen.insert(target.id.clone()) {
            deduped.push(target);
        }
    }

    deduped
}

#[allow(dead_code)]
pub fn systemd_user_service_spec(target: &ActivationTarget) -> Result<SystemdUserServiceSpec> {
    let TargetDetails::SystemdUserService {
        service,
        service_file,
        exec_start,
        executable,
        ..
    } = &target.details
    else {
        return Err(anyhow!(
            "target is not a systemd user service: {}",
            target.id
        ));
    };

    Ok(SystemdUserServiceSpec {
        service: service.clone(),
        service_file: service_file.clone(),
        exec_start: exec_start.clone(),
        executable: executable.clone(),
    })
}

fn discover_path_shim(output: &Path) -> Result<Option<ActivationTarget>> {
    let bin_dir = output.join("bin");
    if !bin_dir.is_dir() {
        return Ok(None);
    }

    let executables = executable_entries(&bin_dir)?;
    if executables.is_empty() {
        return Ok(None);
    }

    let mut path_matches = Vec::new();
    for executable in &executables {
        let Some(name) = executable.file_name() else {
            continue;
        };
        let name = name.to_string_lossy().into_owned();
        path_matches.extend(find_path_matches(output, &name));
    }

    let confidence = if path_matches
        .iter()
        .any(|path_match| path_match.points_to_output)
    {
        "high"
    } else if path_matches.is_empty() {
        "low"
    } else {
        "medium"
    };

    let mut evidence = vec![format!(
        "output has {} executable(s) in bin/",
        executables.len()
    )];
    if path_matches.is_empty() {
        evidence.push("no matching executable currently found in PATH".to_owned());
    } else {
        evidence.push(format!(
            "{} matching PATH entr{} found",
            path_matches.len(),
            if path_matches.len() == 1 { "y" } else { "ies" }
        ));
    }

    Ok(Some(ActivationTarget {
        id: "path-shim".to_owned(),
        kind: "path-shim".to_owned(),
        confidence: confidence.to_owned(),
        supported: true,
        active: false,
        evidence,
        details: TargetDetails::PathShim {
            executables,
            path_matches,
        },
    }))
}

fn discover_systemd_user_services(
    output: &Path,
    dbus_by_systemd: &HashMap<String, Vec<String>>,
) -> Result<Vec<ActivationTarget>> {
    let mut targets = Vec::new();

    for dir in [
        output.join("share/systemd/user"),
        output.join("lib/systemd/user"),
    ] {
        if !dir.is_dir() {
            continue;
        }

        for entry in
            fs::read_dir(&dir).with_context(|| format!("failed to read {}", dir.display()))?
        {
            let entry =
                entry.with_context(|| format!("failed to read entry in {}", dir.display()))?;
            let path = entry.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("service") {
                continue;
            }

            let service = path
                .file_name()
                .ok_or_else(|| anyhow!("service file has no file name: {}", path.display()))?
                .to_string_lossy()
                .into_owned();
            let properties = parse_ini_file(&path)?;
            let exec_start = last_nonempty_value(&properties, "Service", "ExecStart");
            let bus_name = last_nonempty_value(&properties, "Service", "BusName");
            let executable = exec_start.as_deref().and_then(parse_exec_executable);
            let dbus_names = dbus_by_systemd.get(&service).cloned().unwrap_or_default();

            let mut evidence = vec![format!("packaged user service: {}", path.display())];
            if let Some(exec_start) = &exec_start {
                evidence.push(format!("ExecStart={exec_start}"));
            }
            if let Some(bus_name) = &bus_name {
                evidence.push(format!("BusName={bus_name}"));
            }
            for dbus_name in &dbus_names {
                evidence.push(format!("D-Bus service {dbus_name} activates {service}"));
            }

            targets.push(ActivationTarget {
                id: format!("systemd-user:{service}"),
                kind: "systemd-user-service".to_owned(),
                confidence: "high".to_owned(),
                supported: executable.is_some(),
                active: false,
                evidence,
                details: TargetDetails::SystemdUserService {
                    service,
                    service_file: path,
                    exec_start: exec_start.unwrap_or_default(),
                    executable,
                    dbus_names,
                },
            });
        }
    }

    Ok(targets)
}

fn discover_dbus_services(output: &Path) -> Result<Vec<DbusServiceInfo>> {
    let dir = output.join("share/dbus-1/services");
    if !dir.is_dir() {
        return Ok(Vec::new());
    }

    let mut services = Vec::new();
    for entry in fs::read_dir(&dir).with_context(|| format!("failed to read {}", dir.display()))? {
        let entry = entry.with_context(|| format!("failed to read entry in {}", dir.display()))?;
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("service") {
            continue;
        }

        let properties = parse_ini_file(&path)?;
        let Some(name) = last_nonempty_value(&properties, "D-BUS Service", "Name") else {
            continue;
        };
        services.push(DbusServiceInfo {
            name,
            service_file: path,
            exec: last_nonempty_value(&properties, "D-BUS Service", "Exec"),
            systemd_service: last_nonempty_value(&properties, "D-BUS Service", "SystemdService"),
        });
    }

    Ok(services)
}

fn dbus_target(service: DbusServiceInfo) -> ActivationTarget {
    let mut evidence = vec![format!(
        "packaged D-Bus service: {}",
        service.service_file.display()
    )];
    if let Some(exec) = &service.exec {
        evidence.push(format!("Exec={exec}"));
    }
    if let Some(systemd_service) = &service.systemd_service {
        evidence.push(format!("SystemdService={systemd_service}"));
    }

    ActivationTarget {
        id: format!("dbus-service:{}", service.name),
        kind: "dbus-service".to_owned(),
        confidence: "high".to_owned(),
        supported: false,
        active: false,
        evidence,
        details: TargetDetails::DbusService {
            name: service.name,
            service_file: service.service_file,
            exec: service.exec,
            systemd_service: service.systemd_service,
        },
    }
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

fn find_path_matches(output: &Path, name: &str) -> Vec<PathMatch> {
    let path = env::var_os("PATH").unwrap_or_default();
    env::split_paths(&path)
        .filter_map(|dir| {
            let candidate = dir.join(name);
            if fs::symlink_metadata(&candidate).is_err() {
                return None;
            }

            let resolved = candidate.canonicalize().ok();
            let points_to_output = resolved
                .as_ref()
                .is_some_and(|resolved| resolved.starts_with(output));
            Some(PathMatch {
                name: name.to_owned(),
                path: candidate,
                resolved,
                points_to_output,
            })
        })
        .collect()
}

fn parse_ini_file(path: &Path) -> Result<Vec<(String, String, String)>> {
    let text =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let mut section = String::new();
    let mut values = Vec::new();

    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            section = line[1..line.len() - 1].to_owned();
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        values.push((
            section.clone(),
            key.trim().to_owned(),
            value.trim().to_owned(),
        ));
    }

    Ok(values)
}

fn last_nonempty_value(
    values: &[(String, String, String)],
    section: &str,
    key: &str,
) -> Option<String> {
    values
        .iter()
        .rev()
        .find(|(candidate_section, candidate_key, value)| {
            candidate_section == section && candidate_key == key && !value.is_empty()
        })
        .map(|(_, _, value)| value.clone())
}

fn parse_exec_executable(exec_start: &str) -> Option<PathBuf> {
    let token = first_exec_token(exec_start)?;
    let token = token.trim_start_matches(['-', '@', '+', '!', ':']);
    let path = PathBuf::from(token);
    if path.is_absolute() { Some(path) } else { None }
}

fn first_exec_token(exec_start: &str) -> Option<&str> {
    let exec_start = exec_start.trim();
    if exec_start.is_empty() {
        return None;
    }

    if let Some(rest) = exec_start.strip_prefix('"') {
        let end = rest.find('"')?;
        return Some(&rest[..end]);
    }

    exec_start.split_whitespace().next()
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use tempfile::TempDir;

    #[test]
    fn parses_systemd_execstart_executable() {
        assert_eq!(
            super::parse_exec_executable("/nix/store/example/bin/tool --flag").unwrap(),
            std::path::PathBuf::from("/nix/store/example/bin/tool")
        );
        assert_eq!(
            super::parse_exec_executable("\"/nix/store/example/bin/tool with spaces\" --flag")
                .unwrap(),
            std::path::PathBuf::from("/nix/store/example/bin/tool with spaces")
        );
        assert_eq!(
            super::parse_exec_executable("-/nix/store/example/bin/tool").unwrap(),
            std::path::PathBuf::from("/nix/store/example/bin/tool")
        );
    }

    #[test]
    fn discovers_dbus_backed_systemd_user_service() {
        let temp = TempDir::new().unwrap();
        let output = temp.path();
        let executable = output.join("libexec/portal");
        fs::create_dir_all(executable.parent().unwrap()).unwrap();
        fs::write(&executable, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();

        let systemd_dir = output.join("share/systemd/user");
        fs::create_dir_all(&systemd_dir).unwrap();
        fs::write(
            systemd_dir.join("portal.service"),
            format!(
                "\
[Service]
Type=dbus
BusName=org.example.Portal
ExecStart={}
",
                executable.display()
            ),
        )
        .unwrap();

        let dbus_dir = output.join("share/dbus-1/services");
        fs::create_dir_all(&dbus_dir).unwrap();
        fs::write(
            dbus_dir.join("org.example.Portal.service"),
            "\
[D-BUS Service]
Name=org.example.Portal
SystemdService=portal.service
Exec=/ignored
",
        )
        .unwrap();

        let report = super::discover(output).unwrap();

        assert_eq!(report.targets.len(), 1);
        assert_eq!(report.targets[0].id, "systemd-user:portal.service");
        assert!(
            report.targets[0]
                .evidence
                .iter()
                .any(|line| line.contains("D-Bus service org.example.Portal"))
        );
    }
}
