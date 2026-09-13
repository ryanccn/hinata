# SPDX-FileCopyrightText: 2026 Ryan Cao <hello@ryanccn.dev>
#
# SPDX-License-Identifier: GPL-3.0-or-later

{
  dev ? true,
  nodeMajor ? null,
}:

let
  pkgs =
    import
      (builtins.fetchTarball {
        url = "https://github.com/NixOS/nixpkgs/archive/02f5696b0e6097e589076d886b317b83ff0437d7.tar.gz";
        sha256 = "sha256-llGJbC0CcU8DfROr6mZjRJgMLQd/SKwfpzxJH/2lHO4=";
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
