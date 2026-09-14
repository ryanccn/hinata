<!--
SPDX-FileCopyrightText: 2026 Ryan Cao <hello@ryanccn.dev>

SPDX-License-Identifier: GPL-3.0-or-later
-->

# hinata

A JavaScript package manager that builds `node_modules` with Nix.

hinata resolves `package.json` against the npm registry into `hinata.lock`. Each package becomes its own Nix derivation, with its dependencies linked beside it as in pnpm's layout, and install scripts run offline in the Nix sandbox. The project's `node_modules` is a writable directory of links into the store.

Install scripts only run for packages you allow, and can't reach anything outside the Nix sandbox. A package that compiles a native module is built once and reused by every project with the same version and dependencies, and the build can be pushed to a binary cache so CI and other machines never repeat it. Libraries it compiles against come from Nixpkgs instead of whatever is installed on the machine. And because the lockfile already pins everything, apps build in Nix straight from `hinata.lock`, with no dependency hash to keep updated.

## Usage

```sh
hinata install                # resolve, build, and link node_modules
hinata add [-D|-O|-E] <pkg>   # add dependencies and install them
hinata remove <pkg>           # remove dependencies and uninstall them
hinata update [pkg...]        # update dependencies within their ranges
hinata update --nixpkgs       # lock Nixpkgs again
hinata run <script>           # run a package.json script
hinata exec <command>         # run a command with node_modules/.bin on PATH
hinata push <store-uri>       # push packages built by install scripts to a binary cache
hinata gc                     # remove stale GC roots and cached registry metadata
```

## Projects

Without a `hinata.lock`, `hinata install` installs from `pnpm-lock.yaml` (lockfile version 9.0) as long as it matches `package.json`, and leaves both files untouched. `hinata install --save-lock`, `add`, `remove` and `update` write a `hinata.lock`, which takes precedence from then on. `hinata install --frozen-lockfile` fails instead of resolving dependencies or changing `hinata.lock`.

Directories matched by `packages` in `pnpm-workspace.yaml` are resolved together into one lockfile, and each gets its own `node_modules`. Dependencies on other workspace packages must use the `workspace:` protocol (`workspace:*`, `workspace:^1.0.0` or `workspace:name@*`) and are linked to their directories; other ranges always come from the registry.

```yaml
packages:
  - packages/*
  - "!packages/legacy"
```

## Configuration

hinata is configured under `hinata` in `package.json`. `allowBuilds`, `buildInputs` and `patchedDependencies` can also be set in `pnpm-workspace.yaml`, and entries in `package.json` take precedence:

```yaml
allowBuilds:
  esbuild: true
```

Install scripts only run for packages listed in `allowBuilds`. They run in the Nix sandbox, without network access. Scripts that download things, for example into `~/.cache`, can run after linking instead, outside the sandbox and against the read-only package:

```json
{
  "hinata": {
    "allowBuilds": {
      "esbuild": true,
      "puppeteer": "impure"
    }
  }
}
```

Install scripts that compile against system libraries can get them from Nixpkgs, by attribute path, through `buildInputs`. Packages with build inputs must be allowed to build in the sandbox:

```json
{
  "hinata": {
    "allowBuilds": ["canvas"],
    "buildInputs": {
      "canvas": ["cairo", "pango", "pkg-config"]
    }
  }
}
```

Packages are built with the Nixpkgs revision locked in `hinata.lock`. It is locked from the flake reference in `nixpkgs` if there is one, then from the `nixpkgs` input in the project's `flake.lock`, and otherwise from `nixpkgs-unstable`. A revision from `flake.lock` follows that file, and others stay the same until `hinata update --nixpkgs` locks them again:

```json
{
  "hinata": {
    "nixpkgs": "github:NixOS/nixpkgs/nixos-25.05"
  }
}
```

When an install script fails, hinata suggests `"impure"` if the script appears to have needed the network, and `buildInputs` if it appears to have been missing a library, header or `pkg-config`.

Patches listed in `patchedDependencies` are applied with `patch -p1`, as produced by `git diff`, before install scripts run. Keys are `name@version` for one version or `name` for every version, and paths are relative to the project. `hinata.lock` records a hash of each patch, so editing one rebuilds the package.

```json
{
  "hinata": {
    "patchedDependencies": {
      "react@18.3.1": "patches/react.patch"
    }
  }
}
```

## Binary caches

Packages built by install scripts can be pushed to a Nix binary cache after installing, so that machines on the same platform and Node.js major version download them instead of building them again:

```sh
hinata push 's3://my-cache?secret-key=/run/secrets/cache-key'   # any Nix store URI
hinata push --print | cachix push my-cache
```

Projects list the caches to download from in `substituters`, with their public keys:

```json
{
  "hinata": {
    "substituters": {
      "https://my-cache.cachix.org": "my-cache.cachix.org-1:…"
    }
  }
}
```

Nix only uses these for trusted users, or when `trusted-substituters` in `nix.conf` lists them.

## Nix

The flake exposes the library for building apps without a dependency hash:

```nix
(hinata.lib.mkHinata pkgs).buildNodeApp {
  pname = "app";
  version = "1.0.0";
  src = ./.;
  lockFile = ./hinata.lock;
}
```

The library only reads `hinata.lock`, builds with the `pkgs` it is given rather than the locked Nixpkgs, and does not use `substituters`; configure caches for it in `nix.conf` or the flake's `nixConfig`.
