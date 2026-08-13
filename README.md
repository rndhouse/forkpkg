<p align="center">
  <img src="assets/forkpkg-logo.png" alt="forkpkg logo" width="160">
</p>

<h1 align="center">forkpkg</h1>

<p align="center"><strong>Local editable forks for Nix packages.</strong></p>

`forkpkg` creates a local editable fork of a Nix package.

Give it a package such as `nixpkgs#hello`. It copies the source tree that Nix
would build into a local Git repo. When you run `forkpkg build`, Nix rebuilds
the original package using your edited source.

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
forkpkg enable
hello
forkpkg disable
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
