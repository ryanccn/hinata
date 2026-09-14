# SPDX-FileCopyrightText: 2026 Ryan Cao <hello@ryanccn.dev>
#
# SPDX-License-Identifier: GPL-3.0-or-later

{
  dev ? true,
  nodeMajor ? null,
}:

let
  lock = builtins.fromJSON (builtins.readFile ../hinata.lock);

  pkgs =
    import (builtins.fetchTree lock.nixpkgs.locked).outPath
      {
        system = builtins.currentSystem;
        # Explicit, so that user nixpkgs configuration cannot change the build.
        config = { };
        overlays = [ ];
      };

  nodejs =
    if nodeMajor == null then
      pkgs.nodejs
    else
      pkgs."nodejs_${toString nodeMajor}"
        or (throw "hinata: the locked Nixpkgs has no nodejs_${toString nodeMajor}, so it cannot build native addons for the Node.js ${toString nodeMajor} on your PATH; run hinata update --nixpkgs, or set hinata.nixpkgs to a newer revision");
in
(import ./. { inherit pkgs nodejs; }).mkWorkspace {
  lockFile = ../hinata.lock;
  inherit dev;
}
