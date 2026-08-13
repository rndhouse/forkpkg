<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/forkpkg-logo-and-text-dark.png">
    <img alt="forkpkg" src="assets/forkpkg-logo-and-text-light.png" height="48">
  </picture>
</p>

`forkpkg` turns a Nix package into an editable local Git fork of the exact
post-patch source tree Nix builds.

Edit the source like a normal checkout, then enable the fork through Nix so the
package is rebuilt and activated by the original package definition.

## Fork and Edit

This repository includes an example patch for `xdg-desktop-portal-gnome` that
changes the remote desktop permission dialog to show which app is requesting
access.

```sh
forkpkg fork nixpkgs#xdg-desktop-portal-gnome --label remote-interaction
forkpkg apply examples/xdg-desktop-portal-gnome-remote-interaction.patch \
  xdg-desktop-portal-gnome \
  --label remote-interaction
```

Use `build` while iterating; it checks the edited source through Nix and prints
the rebuilt output path without changing what your shell or system uses.

```sh
forkpkg build xdg-desktop-portal-gnome --label remote-interaction
```

## Enable/Disable Forks

Inspect the available activation targets, enable the fork, then disable it when
you want to go back to the previous package:

```sh
forkpkg targets xdg-desktop-portal-gnome --label remote-interaction
forkpkg enable xdg-desktop-portal-gnome \
  --label remote-interaction \
  --backend nixos-module
forkpkg disable xdg-desktop-portal-gnome --label remote-interaction
```

By default, `enable` asks Nix to own activation. Command-line packages use a
Nix profile. NixOS and Home Manager packages can use generated modules and the
normal switch flow. For generated modules, keep the printed module path
imported in your config and switch your system after enabling or disabling the
fork.

## Multiple Forks

The first fork for a package uses the default label. Use `--label` when you want
more than one fork of the same package.

```sh
forkpkg fork nixpkgs#ripgrep --label parser-test
forkpkg build ripgrep --label parser-test
```

## Share Changes

Export a fork into a share artifact, then import it into another matching fork.

```sh
forkpkg export ripgrep --output ripgrep.forkpkg
forkpkg import ripgrep.forkpkg ripgrep
```

## Current Scope

`forkpkg` works best with conventional Nix packages whose source can be replaced
with a local checkout.

Some packages with custom Nix build logic may not work yet.
