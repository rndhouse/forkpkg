# forkpkg

`forkpkg` is a Rust CLI that turns a Nix package into an editable local source
fork that can be rebuilt through the original Nix package definition.

## Usage

```sh
forkpkg fork nixpkgs#ripgrep
forkpkg list
forkpkg list --json
cd ~/.local/share/forkpkg/forks/ripgrep/source

# edit source
forkpkg build
forkpkg info
forkpkg enable
forkpkg status
forkpkg disable
```

Managed forks are stored under `$XDG_DATA_HOME/forkpkg/forks/`, falling back to
`~/.local/share/forkpkg/forks/`.

Each workspace has this shape:

```text
fork/
├── forkpkg.toml
└── source/
```

`source/` is a Git repository. Its first commit is the pristine source tree
after the package's normal Nix `unpackPhase` and `patchPhase`.

## Current Build Strategy

`forkpkg fork` asks Nix to build a helper derivation from the original package
with phases limited to:

```text
unpackPhase -> patchPhase -> installPhase
```

The resulting post-patch source tree is copied into the workspace and committed
as the base Git commit.

`forkpkg build` rebuilds the original package with `overrideAttrs`:

- `src` is replaced with the local source tree.
- `.git` is filtered out before Nix copies the source into the store.
- `patches`, `prePatch`, and `postPatch` are cleared.
- `patchPhase` is replaced with hook-only no-op behavior.
- `unpackPhase` copies the local post-patch tree into `source/`.

This is intended for conventional `stdenv` packages where the persisted tree is
already the right post-patch build input.

## Current Activation Strategy

`forkpkg enable` currently supports a CLI-oriented `path-shim` activation mode.
It rebuilds the fork and symlinks executables from the build output's `bin/`
directory into `~/.local/bin`.

Machine-local activation records live under:

```text
$XDG_STATE_HOME/forkpkg/activations/
```

with fallback:

```text
~/.local/state/forkpkg/activations/
```

If an executable already exists in `~/.local/bin`, forkpkg moves it into the
activation backup directory before creating the symlink. `forkpkg disable`
checks that the symlink still points to the forked output before removing it and
restoring the previous file.

This mode is useful for simple CLI testing, for example `hello`. It is not the
right final activation mechanism for services or NixOS system packages.

## Known v0 Limits

- Multiple source inputs are not modeled.
- Packages where replacing `src` is insufficient may fail.
- Packages deriving important values from the original `src` may fail.
- Custom patch phases that do more than mutate the source tree may fail.
- Cargo dependency changes are not handled; existing Nix cargo vendoring is reused.
- Source filters other than excluding `.git` are not reconstructed.
- Non-`stdenv` packages are not generalized.
- Activation only handles CLI `bin/` outputs via `~/.local/bin` symlinks.
- The nixpkgs revision may be unavailable when `nixpkgs` resolves to a registry
  path flake; in that case forkpkg records the resolved flake URL/path and hash
  metadata that Nix exposes.
