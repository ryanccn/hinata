# SPDX-FileCopyrightText: 2026 Ryan Cao <hello@ryanccn.dev>
#
# SPDX-License-Identifier: GPL-3.0-or-later

{
  pkgs,
  lib ? pkgs.lib,
  nodejs ? pkgs.nodejs,
}:

let
  helpers = ./hinata.sh;
  platform = pkgs.stdenv.hostPlatform;

  hostOs =
    if platform.isDarwin then
      "darwin"
    else if platform.isLinux then
      "linux"
    else if platform.isFreeBSD then
      "freebsd"
    else if platform.isOpenBSD then
      "openbsd"
    else if platform.isWindows then
      "win32"
    else
      platform.parsed.kernel.name;

  hostCpu =
    if platform.isAarch64 then
      "arm64"
    else if platform.isx86_64 then
      "x64"
    else if platform.isx86_32 then
      "ia32"
    else if platform.isAarch32 then
      "arm"
    else
      platform.parsed.cpu.name;

  hostLibc = if platform.isMusl then "musl" else "glibc";

  matches =
    value: allowed:
    let
      excluded = map (lib.removePrefix "!") (lib.filter (lib.hasPrefix "!") allowed);
      included = lib.filter (entry: !lib.hasPrefix "!" entry) allowed;
    in
    !lib.elem value excluded && (included == [ ] || lib.elem value included);

  compatible =
    p:
    matches hostOs (p.os or [ ])
    && matches hostCpu (p.cpu or [ ])
    && (!platform.isLinux || matches hostLibc (p.libc or [ ]));

  loadLock =
    lockFile:
    let
      lock = lib.importJSON lockFile;
      packages =
        if lock.version == 1 then
          lib.filterAttrs (_: compatible) lock.packages
        else
          throw "hinata: ${toString lockFile} has unsupported version ${toString lock.version}";

      cycleMembers = lib.listToAttrs (
        lib.concatLists (
          lib.imap0 (
            group: members:
            lib.imap0 (
              index: id:
              lib.nameValuePair id {
                group = "cycle#${toString group}";
                prefix = "/${toString index}";
              }
            ) members
          ) lock.sccs
        )
      );
      groupOf = id: cycleMembers.${id}.group or id;
      prefixOf = id: cycleMembers.${id}.prefix or "";

      resolve =
        who: required: optional:
        let
          missing = lib.filterAttrs (_: id: !(packages ? ${id})) required;
        in
        if missing != { } then
          throw "hinata: ${who} requires ${lib.concatStringsSep ", " (lib.attrValues missing)}, which cannot be installed on ${platform.system}"
        else
          required // lib.filterAttrs (_: id: packages ? ${id}) optional;

      depsOf = id: resolve id (packages.${id}.deps or { }) (packages.${id}.optionalDeps or { });

      rootOf = id: "${groups.${groupOf id}}${prefixOf id}/node_modules/${packages.${id}.name}";

      # A derivation cannot refer to its own output path.
      targetFrom =
        from: to:
        if groupOf from == groupOf to then
          "$out${prefixOf to}/node_modules/${packages.${to}.name}"
        else
          rootOf to;

      fetchPackage =
        p:
        (pkgs.fetchurl {
          name = "${lib.strings.sanitizeDerivationName "${p.name}-${p.version}"}.tgz";
          url = p.url;
          hash = p.integrity;
        }).overrideAttrs
          (_: {
            allowSubstitutes = false;
          });

      patchFile =
        p:
        let
          file = builtins.path {
            path = dirOf lockFile + "/${p.patch.path}";
            name = lib.strings.sanitizeDerivationName "${p.name}-${p.version}.patch";
          };
        in
        if builtins.hashFile "sha256" file == p.patch.hash then
          file
        else
          throw "hinata: ${p.patch.path} has changed since ${toString lockFile} was written; run hinata install";

      # Dependencies are linked beside the package so that relative paths from native addons to
      # sibling packages resolve.
      installMember =
        id:
        let
          p = packages.${id};
        in
        ''
          dest="$out${prefixOf id}/node_modules/${p.name}"
          unpackNpm ${fetchPackage p} "$dest"
          ${lib.optionalString (p ? patch) ''patch -p1 --no-backup-if-mismatch -d "$dest" -i ${patchFile p}''}
          ${lib.concatStrings (
            lib.mapAttrsToList (alias: to: ''
              linkDep "$out${prefixOf id}" "${alias}" "${targetFrom id to}"
            '') (depsOf id)
          )}
          ${lib.optionalString (p.hasBin or false) ''markBinsExecutable "$dest"''}
        '';

      buildInputOf =
        id: attr:
        lib.attrByPath (lib.splitString "." attr)
          (throw "hinata: ${id} has build input ${attr}, which is not in nixpkgs")
          pkgs;

      buildMember =
        id:
        lib.optionalString (packages.${id}.build or false) ''
          runInstallScripts "$out${prefixOf id}/node_modules/${packages.${id}.name}" "$out${prefixOf id}/node_modules"
        '';

      mkGroup =
        key: members:
        let
          first = packages.${lib.head members};
          needsBuild = lib.any (id: packages.${id}.build or false) members;
          name = lib.strings.sanitizeDerivationName (
            if lib.length members == 1 then
              "${first.name}-${first.version}"
            else
              "hinata-cycle-${first.name}-${first.version}"
          );
          script = ''
            source ${helpers}
            ${lib.concatMapStrings installMember members}
            ${lib.concatMapStrings buildMember members}
          '';
        in
        if needsBuild then
          pkgs.runCommandCC name {
            nativeBuildInputs = [
              pkgs.jq
              nodejs
              pkgs.python3
            ]
            # node-gyp on darwin needs libtool and xcrun, which the darwin stdenv lacks.
            ++ lib.optionals platform.isDarwin [
              pkgs.cctools
              pkgs.xcbuild
            ];
            buildInputs = lib.concatMap (id: map (buildInputOf id) (packages.${id}.buildInputs or [ ])) members;
            npm_config_nodedir = nodejs;
            nodeGyp = "${nodejs}/lib/node_modules/npm/node_modules/node-gyp/bin/node-gyp.js";
          } script
        else
          # Sourcing the stdenv setup costs more than unpacking and linking most packages.
          derivation {
            inherit name script;
            system = pkgs.stdenv.buildPlatform.system;
            builder = pkgs.stdenv.shell;
            args = [
              "-euo"
              "pipefail"
              "-c"
              ''source "$scriptPath"''
            ];
            passAsFile = [ "script" ];
            PATH = lib.makeBinPath [
              pkgs.coreutils
              pkgs.gnupatch
              pkgs.gnutar
              pkgs.gzip
              pkgs.jq
            ];
            preferLocalBuild = true;
            allowSubstitutes = false;
          };

      groups = lib.mapAttrs mkGroup (lib.groupBy groupOf (lib.attrNames packages));

      importerIds =
        {
          importer ? ".",
          dev ? true,
        }:
        let
          imp =
            lock.importers.${importer} or (throw "hinata: no importer ${importer} in ${toString lockFile}");
          who = "importer ${importer}";
        in
        {
          dependencies = resolve who (imp.dependencies or { }) { };
          devDependencies = lib.optionalAttrs dev (resolve who (imp.devDependencies or { }) { });
          optionalDependencies = resolve who { } (imp.optionalDependencies or { });
        };

      importerRoots = args: lib.mapAttrs (_: lib.mapAttrs (_: rootOf)) (importerIds args);

      buildsFor =
        dev:
        let
          roots = lib.concatMap (
            importer:
            lib.concatMap lib.attrValues (
              lib.attrValues (importerIds {
                inherit importer dev;
              })
            )
          ) (lib.attrNames lock.importers);

          reachable = builtins.genericClosure {
            startSet = map (id: { key = id; }) roots;
            operator = { key }: map (id: { key = id; }) (lib.attrValues (depsOf key));
          };
        in
        lib.unique (
          map (item: groups.${groupOf item.key}) (
            lib.filter (item: packages.${item.key}.build or false) reachable
          )
        );
    in
    {
      inherit
        lock
        packages
        importerRoots
        buildsFor
        rootOf
        ;

      nodeModulesFor =
        {
          importer ? ".",
          dev ? true,
        }:
        let
          roots = importerRoots { inherit importer dev; };
        in
        pkgs.runCommand "node-modules"
          {
            nativeBuildInputs = [ pkgs.jq ];
            preferLocalBuild = true;
            allowSubstitutes = false;
          }
          ''
            source ${helpers}
            mkdir -p "$out/node_modules"
            ${lib.concatStrings (
              lib.mapAttrsToList (alias: root: ''
                linkDep "$out" "${alias}" "${root}"
              '') (roots.optionalDependencies // roots.devDependencies // roots.dependencies)
            )}
            linkBins "$out/node_modules"
          '';
    };

  mkNodeModules =
    {
      lockFile,
      importer ? ".",
      dev ? true,
    }:
    (loadLock lockFile).nodeModulesFor { inherit importer dev; };

  mkWorkspace =
    {
      lockFile,
      dev ? true,
    }:
    let
      loaded = loadLock lockFile;
    in
    pkgs.runCommand "hinata-workspace"
      {
        importers = builtins.toJSON (
          lib.mapAttrs (importer: _: loaded.importerRoots { inherit importer dev; }) loaded.lock.importers
        );
        impureBuilds = builtins.toJSON (
          lib.mapAttrs (id: _: loaded.rootOf id) (
            lib.filterAttrs (_: p: p.impureBuild or false) loaded.packages
          )
        );
        builds = builtins.toJSON (loaded.buildsFor dev);
        passAsFile = [
          "importers"
          "impureBuilds"
          "builds"
        ];
        preferLocalBuild = true;
        allowSubstitutes = false;
      }
      ''
        mkdir -p "$out"
        cp "$importersPath" "$out/importers.json"
        cp "$impureBuildsPath" "$out/impure-builds.json"
        cp "$buildsPath" "$out/builds.json"
      '';

  buildNodeApp =
    {
      lockFile,
      importer ? ".",
      buildScript ? "build",
      distDir ? "dist",
      nativeBuildInputs ? [ ],
      ...
    }@args:
    let
      nodeModules = mkNodeModules { inherit lockFile importer; };
    in
    pkgs.stdenv.mkDerivation (
      builtins.removeAttrs args [
        "lockFile"
        "importer"
        "buildScript"
        "distDir"
      ]
      // {
        nativeBuildInputs = [ nodejs ] ++ nativeBuildInputs;

        configurePhase =
          args.configurePhase or ''
            runHook preConfigure
            source ${helpers}
            linkNodeModules ${nodeModules}/node_modules node_modules
            export HOME="$TMPDIR" npm_config_update_notifier=false
            runHook postConfigure
          '';

        buildPhase =
          args.buildPhase or ''
            runHook preBuild
            npm run ${lib.escapeShellArg buildScript}
            runHook postBuild
          '';

        installPhase =
          args.installPhase or ''
            runHook preInstall
            cp -r ${lib.escapeShellArg distDir} "$out"
            runHook postInstall
          '';

        passthru = (args.passthru or { }) // {
          inherit nodeModules;
        };
      }
    );
in
{
  inherit mkNodeModules mkWorkspace buildNodeApp;
}
