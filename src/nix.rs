use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::metadata::Metadata;

#[derive(Debug, Clone)]
pub struct Installable {
    pub original: String,
    pub flake_ref: String,
    pub attribute: String,
    pub attr_path: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ResolvedPackage {
    pub installable: Installable,
    pub system: String,
    pub package_name: Option<String>,
    pub package_pname: Option<String>,
    pub package_version: Option<String>,
    pub derivation: String,
    pub output: String,
    pub source: Option<String>,
    pub source_revision: Option<String>,
    pub source_hash: Option<String>,
    pub flake: FlakeIdentity,
    pub source_store: Option<StorePathInfo>,
}

#[derive(Debug, Clone, Default)]
pub struct FlakeIdentity {
    pub revision: Option<String>,
    pub last_modified: Option<u64>,
    pub locked_nar_hash: Option<String>,
    pub resolved_url: Option<String>,
    pub path: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct StorePathInfo {
    pub nar_hash: Option<String>,
    pub ca: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PackageEval {
    system: String,
    #[serde(rename = "packageName")]
    package_name: Option<String>,
    #[serde(rename = "packagePname")]
    package_pname: Option<String>,
    #[serde(rename = "packageVersion")]
    package_version: Option<String>,
    derivation: String,
    output: String,
    source: Option<String>,
    #[serde(rename = "sourceRevision")]
    source_revision: Option<String>,
    #[serde(rename = "sourceHash")]
    source_hash: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PathInfoEntry {
    #[serde(rename = "narHash")]
    nar_hash: Option<String>,
    ca: Option<String>,
}

pub fn parse_installable(input: &str) -> Result<Installable> {
    let (flake_ref, attribute) = input
        .split_once('#')
        .ok_or_else(|| anyhow!("expected a flake installable like nixpkgs#ripgrep"))?;

    if flake_ref.trim().is_empty() || attribute.trim().is_empty() {
        bail!("expected a flake installable like nixpkgs#ripgrep");
    }

    if attribute.contains('^') {
        bail!("output-selected installables are not supported yet: {input}");
    }

    let attr_path: Vec<String> = attribute
        .split('.')
        .filter(|part| !part.is_empty())
        .map(ToOwned::to_owned)
        .collect();

    if attr_path.is_empty() {
        bail!("package attribute is empty in installable: {input}");
    }

    Ok(Installable {
        original: input.to_owned(),
        flake_ref: flake_ref.to_owned(),
        attribute: attribute.to_owned(),
        attr_path,
    })
}

pub fn resolve_installable(input: &str) -> Result<ResolvedPackage> {
    let installable = parse_installable(input)?;
    let eval: PackageEval = nix_json(
        &["eval", "--json", "--impure", "--expr"],
        &[package_eval_expr(&installable)],
    )?;
    let flake = flake_metadata(&installable.flake_ref)?;
    let source_store = match eval.source.as_deref() {
        Some(path) => path_info(path)?,
        None => None,
    };

    Ok(ResolvedPackage {
        installable,
        system: eval.system,
        package_name: eval.package_name,
        package_pname: eval.package_pname,
        package_version: eval.package_version,
        derivation: eval.derivation,
        output: eval.output,
        source: eval.source,
        source_revision: eval.source_revision,
        source_hash: eval.source_hash,
        flake,
        source_store,
    })
}

pub fn materialize_post_patch_source(installable: &Installable) -> Result<PathBuf> {
    let expr = source_materializer_expr(installable);
    nix_build_expr(&expr).context("failed to materialize post-patch source")
}

pub fn build_local_source(metadata: &Metadata, source_path: &Path) -> Result<PathBuf> {
    let expr = build_expr(metadata, source_path)?;
    nix_build_expr(&expr).context("failed to build local fork")
}

pub fn path_info(path: &str) -> Result<Option<StorePathInfo>> {
    let value: BTreeMap<String, Option<PathInfoEntry>> =
        nix_json(&["path-info", "--json"], &[path.to_owned()])?;
    Ok(value
        .into_values()
        .next()
        .flatten()
        .map(|entry| StorePathInfo {
            nar_hash: entry.nar_hash,
            ca: entry.ca,
        }))
}

fn flake_metadata(flake_ref: &str) -> Result<FlakeIdentity> {
    let value: Value = nix_json(&["flake", "metadata", "--json"], &[flake_ref.to_owned()])?;

    let locked = value.get("locked").unwrap_or(&Value::Null);
    let revision = string_at(locked, "rev").or_else(|| string_at(&value, "rev"));
    let last_modified = u64_at(locked, "lastModified").or_else(|| u64_at(&value, "lastModified"));
    let locked_nar_hash = string_at(locked, "narHash");
    let resolved_url = string_at(&value, "resolvedUrl").or_else(|| string_at(&value, "url"));
    let path = string_at(&value, "path");

    Ok(FlakeIdentity {
        revision,
        last_modified,
        locked_nar_hash,
        resolved_url,
        path,
    })
}

fn package_eval_expr(installable: &Installable) -> String {
    format!(
        r#"
let
  flake = builtins.getFlake {flake_ref};
  pkgs = flake.legacyPackages.${{builtins.currentSystem}};
  lib = pkgs.lib;
  pkg = lib.attrByPath {attr_path} (throw {attribute_message}) pkgs;
  src = pkg.src or null;
  srcIsAttrs = builtins.isAttrs src;
in {{
  system = builtins.currentSystem;
  packageName = pkg.name or null;
  packagePname = pkg.pname or null;
  packageVersion = pkg.version or null;
  derivation = toString pkg.drvPath;
  output = toString pkg.outPath;
  source = if src == null then null else toString src;
  sourceRevision = if srcIsAttrs && src ? rev then src.rev else null;
  sourceHash =
    if srcIsAttrs && src ? outputHash then src.outputHash
    else if srcIsAttrs && src ? hash then src.hash
    else null;
}}
"#,
        flake_ref = nix_string(&installable.flake_ref),
        attr_path = nix_string_list(&installable.attr_path),
        attribute_message = attribute_message(&installable.attribute),
    )
}

fn source_materializer_expr(installable: &Installable) -> String {
    format!(
        r#"
let
  flake = builtins.getFlake {flake_ref};
  pkgs = flake.legacyPackages.${{builtins.currentSystem}};
  lib = pkgs.lib;
  pkg = lib.attrByPath {attr_path} (throw {attribute_message}) pkgs;
in
  pkg.overrideAttrs (old: {{
    name = ((old.pname or old.name) + "-forkpkg-source");
    outputs = [ "out" ];
    phases = [ "unpackPhase" "patchPhase" "installPhase" ];
    installPhase = "runHook preInstall\nmkdir -p \"$out\"\nshopt -s dotglob nullglob\ncp -a . \"$out/\"\nrunHook postInstall\n";
    dontFixup = true;
    doCheck = false;
    doInstallCheck = false;
  }})
"#,
        flake_ref = nix_string(&installable.flake_ref),
        attr_path = nix_string_list(&installable.attr_path),
        attribute_message = attribute_message(&installable.attribute),
    )
}

fn build_expr(metadata: &Metadata, source_path: &Path) -> Result<String> {
    let installable = Installable {
        original: metadata.package.installable.clone(),
        flake_ref: metadata.package.flake_ref.clone(),
        attribute: metadata.package.attribute.clone(),
        attr_path: metadata
            .package
            .attribute
            .split('.')
            .filter(|part| !part.is_empty())
            .map(ToOwned::to_owned)
            .collect(),
    };

    if installable.attr_path.is_empty() {
        bail!("metadata package attribute is empty");
    }

    let source_path = source_path
        .canonicalize()
        .with_context(|| format!("failed to resolve {}", source_path.display()))?;
    let source_string = source_path
        .to_str()
        .ok_or_else(|| anyhow!("source path is not valid UTF-8: {}", source_path.display()))?;
    let local_name = format!(
        "forkpkg-local-source-{}",
        metadata
            .package
            .pname
            .as_deref()
            .or(metadata.package.name.as_deref())
            .unwrap_or("package")
    );

    Ok(format!(
        r#"
let
  flake = builtins.getFlake {flake_ref};
  pkgs = flake.legacyPackages.${{{system}}};
  lib = pkgs.lib;
  pkg = lib.attrByPath {attr_path} (throw {attribute_message}) pkgs;
  localSource = builtins.path {{
    path = {source_path};
    name = {local_name};
    filter = path: type: baseNameOf path != ".git";
  }};
in
  pkg.overrideAttrs (old: {{
    src = localSource;
    patches = [];
    prePatch = "";
    postPatch = "";
    patchPhase = "runHook prePatch\nrunHook postPatch\n";
    unpackPhase = "runHook preUnpack\ncp -a --reflink=auto \"$src\" source\nchmod -R u+w source\nsourceRoot=source\nrunHook postUnpack\n";
  }})
"#,
        flake_ref = nix_string(&installable.flake_ref),
        system = nix_string(&metadata.package.system),
        attr_path = nix_string_list(&installable.attr_path),
        attribute_message = attribute_message(&installable.attribute),
        source_path = nix_string(source_string),
        local_name = nix_string(&local_name),
    ))
}

fn nix_build_expr(expr: &str) -> Result<PathBuf> {
    let mut command = nix_command();
    command.args([
        "build",
        "--impure",
        "--no-link",
        "--print-out-paths",
        "--expr",
        expr,
    ]);
    command.stderr(Stdio::inherit());

    let output = command.output().context("failed to execute nix build")?;
    if !output.status.success() {
        return Err(anyhow!("nix build failed with status {}", output.status));
    }

    let stdout =
        String::from_utf8(output.stdout).context("nix build output was not valid UTF-8")?;
    let first = stdout
        .lines()
        .find(|line| !line.trim().is_empty())
        .ok_or_else(|| anyhow!("nix build did not print an output path"))?;
    Ok(PathBuf::from(first.trim()))
}

fn nix_json<T>(args: &[&str], trailing: &[String]) -> Result<T>
where
    T: DeserializeOwned,
{
    let mut command = nix_command();
    command.args(args);
    for arg in trailing {
        command.arg(arg);
    }

    let output = command.output().context("failed to execute nix")?;
    if !output.status.success() {
        return Err(anyhow!(
            "nix failed with status {}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    serde_json::from_slice(&output.stdout).context("failed to parse nix JSON output")
}

fn nix_command() -> Command {
    let mut command = Command::new("nix");
    command.args(["--extra-experimental-features", "nix-command flakes"]);
    command
}

fn nix_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    let mut chars = value.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '$' if chars.peek() == Some(&'{') => out.push_str("\\$"),
            _ => out.push(ch),
        }
    }
    out.push('"');
    out
}

fn attribute_message(attribute: &str) -> String {
    nix_string(&format!("package attribute not found: {attribute}"))
}

fn nix_string_list(values: &[String]) -> String {
    let values = values
        .iter()
        .map(|value| nix_string(value))
        .collect::<Vec<_>>()
        .join(" ");
    format!("[ {values} ]")
}

fn string_at(value: &Value, key: &str) -> Option<String> {
    value.get(key)?.as_str().map(ToOwned::to_owned)
}

fn u64_at(value: &Value, key: &str) -> Option<u64> {
    value.get(key)?.as_u64()
}

#[cfg(test)]
mod tests {
    use super::{attribute_message, nix_string, parse_installable};

    #[test]
    fn parses_simple_nixpkgs_installable() {
        let installable = parse_installable("nixpkgs#ripgrep").unwrap();
        assert_eq!(installable.flake_ref, "nixpkgs");
        assert_eq!(installable.attribute, "ripgrep");
        assert_eq!(installable.attr_path, ["ripgrep"]);
    }

    #[test]
    fn parses_nested_attribute_path() {
        let installable = parse_installable("nixpkgs#foo.bar-baz").unwrap();
        assert_eq!(installable.attr_path, ["foo", "bar-baz"]);
    }

    #[test]
    fn rejects_non_flake_installable() {
        assert!(parse_installable("ripgrep").is_err());
    }

    #[test]
    fn rejects_output_selected_installable() {
        assert!(parse_installable("nixpkgs#ripgrep^man").is_err());
    }

    #[test]
    fn nix_strings_escape_interpolation_and_quotes() {
        assert_eq!(nix_string("a\"b\\${c}\n"), "\"a\\\"b\\\\\\${c}\\n\"");
    }

    #[test]
    fn attribute_messages_are_nix_strings() {
        assert_eq!(
            attribute_message("bad\"pkg"),
            "\"package attribute not found: bad\\\"pkg\""
        );
    }
}
