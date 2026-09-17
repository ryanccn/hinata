<!--
SPDX-FileCopyrightText: 2026 Ryan Cao <hello@ryanccn.dev>

SPDX-License-Identifier: GPL-3.0-or-later
-->

# hinata

A JavaScript package manager that builds `node_modules` with Nix.

hinata resolves `package.json` against the npm registry into `hinata.lock`. Each package becomes its own Nix derivation, with its dependencies linked beside it as in pnpm's layout, and the project's `node_modules` is a writable directory of links into the store. A package with install scripts is built once and reused by every project with the same version and dependencies, and apps build in Nix straight from `hinata.lock`, with no dependency hash to keep updated.

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

## Security

Installing a project doesn't let it or its dependencies reach outside the Nix sandbox without asking:

- Install scripts only run for packages in `allowBuilds`, in the sandbox and without network access.
- Install scripts that run outside the sandbox, and binary caches that the project lists, need approval, which hinata asks for again when they change. `--trust` approves them without asking, for example in CI.
- The Nixpkgs that a project chooses is evaluated purely, so it cannot read files outside the Nix store or the environment.
- Package names, bins and integrity hashes in lockfiles are validated before they reach paths or build scripts.

This does not cover code that the project runs later, such as scripts run with `hinata run`, or builds when the Nix sandbox is off or unavailable, as it is by default on macOS.

## Projects

Without a `hinata.lock`, `hinata install` installs from `pnpm-lock.yaml` (lockfile version 9.0) as long as it matches `package.json`, and leaves both files untouched. `hinata install --save-lock`, `add`, `remove` and `update` write a `hinata.lock`, which takes precedence from then on. `hinata install --frozen-lockfile` fails instead of resolving dependencies or changing `hinata.lock`.

Directories matched by `packages` in `pnpm-workspace.yaml` are resolved together into one lockfile, and each gets its own `node_modules`. Dependencies on other workspace packages must use the `workspace:` protocol (`workspace:*`, `workspace:^1.0.0` or `workspace:name@*`) and are linked to their directories; other ranges always come from the registry.

```yaml
packages:
  - packages/*
  - "!packages/legacy"
```

hinata is configured under `hinata` in `package.json`. `allowBuilds`, `buildInputs` and `patchedDependencies` can also be set in `pnpm-workspace.yaml`, where entries in `package.json` take precedence.

## Install scripts

Install scripts only run for packages listed in `allowBuilds`. Scripts that download things, for example into `~/.cache`, can run after linking instead, outside the sandbox and against the read-only package, once approved:

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

## Toolchain

Packages are built with the Nixpkgs revision locked in `hinata.lock`. It is locked from the flake reference in `nixpkgs` if there is one, then from the `nixpkgs` input in the project's `flake.lock`, and otherwise from `nixpkgs-unstable`. A revision from `flake.lock` follows that file, and others stay the same until `hinata update --nixpkgs` locks them again:

```json
{
  "hinata": {
    "nixpkgs": "github:NixOS/nixpkgs/nixos-25.05"
  }
}
```

`devEngines` locks Node.js from the same revision: the newest release matching the range, or failing that, the newest in a major version it allows. It builds install scripts and is linked into `node_modules/.bin`. Otherwise, native addons are built for the `node` on `PATH`:

```json
{
  "devEngines": {
    "runtime": {
      "name": "node",
      "version": "^22.18.0"
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

Nix only uses these for trusted users, or when `trusted-substituters` in `nix.conf` lists them, and hinata only passes them on once approved.

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
