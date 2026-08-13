<h1 align="center">forkpkg</h1>

<p align="center"><strong>Local editable forks for Nix packages.</strong></p>

<p align="center">
  <img src="assets/forkpkg-logo.png" alt="forkpkg logo" width="160">
</p>

`forkpkg` creates a local editable fork of a Nix package.

Give it a package such as `nixpkgs#hello`. It copies the source tree that Nix
would build into a local Git repo. When you run `forkpkg build`, Nix rebuilds
the original package using your edited source.

```sh
forkpkg fork nixpkgs#hello
```

Then change into the `source:` directory printed by `forkpkg` and edit files:

```sh
forkpkg build
```

Managed forks are selected by package name and optional label:

```sh
forkpkg info ripgrep
forkpkg info xdg-desktop-portal-gnome --label remote-interaction
```

For command-line tools, you can use the rebuilt version from your shell:

```sh
forkpkg enable
hello
forkpkg disable
```

By default, `enable` asks Nix to own activation. Command-line packages use
`nix profile add`; `disable` removes the fork from that profile. Packages that
belong to NixOS or Home Manager can use generated overlay modules and the
normal `nixos-rebuild switch` or `home-manager switch` flow.

Direct legacy activation modes are still useful for experimentation and cleanup:
`path-shim` creates links in `~/.local/bin`, and `systemd-user-service` writes a
user systemd override. They are not the default activation path.

## Current Scope

`forkpkg` works best with normal command-line packages and conventional NixOS
packages whose source can be replaced with a local post-patch tree.

Some packages with custom Nix build logic may not work yet.
