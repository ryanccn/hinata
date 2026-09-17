# SPDX-FileCopyrightText: 2026 Ryan Cao <hello@ryanccn.dev>
#
# SPDX-License-Identifier: GPL-3.0-or-later

{
  outputs =
    _:
    let
      options = builtins.fromJSON (builtins.readFile ./options.json);
      nixpkgs = builtins.fetchTree options.nixpkgs;
      lib = import "${nixpkgs}/lib";
    in
    {
      legacyPackages = lib.genAttrs lib.systems.flakeExposed (
        system:
        let
          pkgs = import nixpkgs {
            inherit system;
            config = { };
            overlays = [ ];
          };

          inherit (options) nodeMajor;
          nodejs =
            if nodeMajor == null then
              pkgs.nodejs
            else
              pkgs."nodejs_${toString nodeMajor}"
                or (throw "hinata: the locked Nixpkgs has no nodejs_${toString nodeMajor}, so it cannot build native addons for the Node.js ${toString nodeMajor} on your PATH; run hinata update --nixpkgs, or set hinata.nixpkgs to a newer revision");

          # Removed versions remain as throwing aliases, and insecure ones are unavailable.
          usable =
            name:
            let
              result = builtins.tryEval (pkgs.${name}.meta.available && builtins.isString pkgs.${name}.version);
            in
            builtins.match "nodejs_[0-9]+" name != null && result.success && result.value;
        in
        {
          workspace = (import ./nix_support { inherit pkgs nodejs; }).mkWorkspace {
            lockFile = ./hinata.lock;
            inherit (options) dev;
          };

          nodeVersions = lib.mapAttrs (_: package: package.version) (
            lib.filterAttrs (name: _: usable name) pkgs
          );
        }
      );
    };
}
