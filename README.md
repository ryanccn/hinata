<!--
SPDX-FileCopyrightText: 2026 Ryan Cao <hello@ryanccn.dev>

SPDX-License-Identifier: GPL-3.0-or-later
-->

# hinata

A JavaScript package manager that builds `node_modules` with Nix.

hinata resolves `package.json` against the npm registry into `hinata.lock`. Each package becomes its own Nix derivation, with its dependencies linked beside it as in pnpm's layout, and install scripts run offline in the Nix sandbox. The project's `node_modules` is a writable directory of links into the store.

## Usage

```sh
hinata install            # resolve, build, and link node_modules
hinata add [-D|-O|-E] <pkg>
hinata remove <pkg>
hinata update [pkg...]
hinata run <script>
hinata exec <command>
```

Install scripts only run for packages listed in `package.json`:

```json
{
  "hinata": {
    "allowBuilds": ["esbuild"]
  }
}
```

Install scripts run in the Nix sandbox, without network access. Scripts that download things, for example into `~/.cache`, can run after linking instead, outside the sandbox and against the read-only package:

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

`allowBuilds` in `pnpm-workspace.yaml` is honored as well, and entries in `package.json` take precedence:

```yaml
allowBuilds:
  esbuild: true
```

Install scripts that compile against system libraries can get them from nixpkgs, by attribute path. Packages with build inputs must be allowed to build in the sandbox:

```json
{
  "hinata": {
    "allowBuilds": ["canvas"],
    "buildInputs": { "canvas": ["cairo", "pango", "pkg-config"] }
  }
}
```

When an install script fails, hinata suggests `"impure"` if the script appears to have needed the network, and `buildInputs` if it appears to have been missing a library, header or `pkg-config`.

## Patches

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

`patchedDependencies` in `pnpm-workspace.yaml` is honored as well, and entries in `package.json` take precedence.

## Workspaces

Directories matched by `packages` in `pnpm-workspace.yaml` are resolved together into one lockfile, and each gets its own `node_modules`. Dependencies on other workspace packages must use the `workspace:` protocol (`workspace:*`, `workspace:^1.0.0` or `workspace:name@*`) and are linked to their directories; other ranges always come from the registry.

```yaml
packages:
  - packages/*
  - "!packages/legacy"
```

## pnpm

Without a `hinata.lock`, `hinata install` installs from `pnpm-lock.yaml` (lockfile version 9.0) as long as it matches `package.json`, and leaves both files untouched. `hinata install --save-lock`, `add`, `remove` and `update` write a `hinata.lock`, which takes precedence from then on. The Nix library only reads `hinata.lock`.

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

## Missing

- `workspace:` dependencies on paths, and injected workspace dependencies
- Running commands from inside a workspace package rather than the root
- Git, file and URL dependencies
- `.npmrc` registries and authentication
- `overrides` and `packageExtensions`
- Packages that rely on undeclared dependencies being hoisted
