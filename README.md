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
{ "hinata": { "allowBuilds": ["esbuild"] } }
```

`pnpm.allowBuilds` is honored as well.

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

- Workspaces and `workspace:` dependencies
- Git, file and URL dependencies
- `.npmrc` registries and authentication
- `overrides` and `packageExtensions`
- Packages that rely on undeclared dependencies being hoisted
- Install scripts that download binaries
- A metadata cache
