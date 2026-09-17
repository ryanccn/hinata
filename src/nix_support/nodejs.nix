# SPDX-FileCopyrightText: 2026 Ryan Cao <hello@ryanccn.dev>
#
# SPDX-License-Identifier: GPL-3.0-or-later

let
  locked = builtins.fromJSON (builtins.getEnv "HINATA_NIXPKGS");

  pkgs = import (builtins.fetchTree locked).outPath {
    system = builtins.currentSystem;
    config = { };
    overlays = [ ];
  };

  names = builtins.filter (name: builtins.match "nodejs_[0-9]+" name != null) (
    builtins.attrNames pkgs
  );

  # Removed versions remain as throwing aliases, and insecure ones are unavailable.
  usable =
    name:
    let
      result = builtins.tryEval (pkgs.${name}.meta.available && builtins.isString pkgs.${name}.version);
    in
    result.success && result.value;
in
builtins.listToAttrs (
  map (name: {
    inherit name;
    value = pkgs.${name}.version;
  }) (builtins.filter usable names)
)
