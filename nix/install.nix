# SPDX-FileCopyrightText: 2026 Ryan Cao <hello@ryanccn.dev>
#
# SPDX-License-Identifier: GPL-3.0-or-later

{
  dev ? true,
  nodeMajor ? null,
}:

let
  flakeLock = builtins.fromJSON (builtins.readFile ../flake.lock);
  nixpkgs = flakeLock.nodes.${flakeLock.nodes.${flakeLock.root}.inputs.nixpkgs}.locked;
  pkgs =
    import
      (builtins.fetchTarball {
        url = "https://github.com/${nixpkgs.owner}/${nixpkgs.repo}/archive/${nixpkgs.rev}.tar.gz";
        sha256 = nixpkgs.narHash;
      })
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
        or (throw "hinata: the pinned nixpkgs has no nodejs_${toString nodeMajor}, so it cannot build native addons for the Node.js ${toString nodeMajor} on your PATH");
in
(import ./. { inherit pkgs nodejs; }).mkWorkspace {
  lockFile = ../hinata.lock;
  inherit dev;
}
