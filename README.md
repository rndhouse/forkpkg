# forkpkg

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

For command-line tools, you can use the rebuilt version from your shell:

```sh
forkpkg enable
hello
forkpkg disable
```

`enable` links the rebuilt command into `~/.local/bin`. `disable` removes that
link and restores the previous command when there was one.

## Current Scope

`forkpkg` works best with normal command-line packages.

It does not handle NixOS services or system activation. Some packages with
custom Nix build logic may not work yet.
