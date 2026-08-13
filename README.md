<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/forkpkg-logo-and-text-dark.png">
    <img alt="forkpkg" src="assets/forkpkg-logo-and-text-light.png" height="48">
  </picture>
</p>

`forkpkg` turns a Nix package into an editable local Git fork of the exact
post-patch source tree Nix builds.

Edit the source like a normal checkout, then rebuild it through the original
Nix package definition so dependencies, build flags, outputs, and activation
stay Nix-owned.

## Edit and Build

```sh
forkpkg fork nixpkgs#hello
```

Then change into the `source:` directory printed by `forkpkg` and edit files:

```sh
forkpkg build
```

## Use the Rebuild

For command-line tools, you can use the rebuilt version from your shell:

```sh
forkpkg enable hello
hello
forkpkg disable hello
```

By default, `enable` asks Nix to own activation. Command-line packages use a
Nix profile. NixOS and Home Manager packages can use generated modules and the
normal switch flow.

Run `forkpkg targets` to see the activation choices for a built fork.

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
