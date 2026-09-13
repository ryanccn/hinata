# SPDX-FileCopyrightText: 2026 Ryan Cao <hello@ryanccn.dev>
#
# SPDX-License-Identifier: GPL-3.0-or-later

{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    ferrix.url = "github:ryanccn/ferrix";
  };

  outputs =
    { ferrix, ... }@inputs:
    ferrix.lib.mkFlake inputs {
      root = ./.;

      flake.lib = {
        mkHinata = pkgs: import ./src/nix_support { inherit pkgs; };
      };
    };
}
