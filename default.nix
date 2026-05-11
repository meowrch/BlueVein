{ pkgs ? import <nixpkgs> { } }:
{
  package = pkgs.callPackage ./nix/package.nix { };
  nixosModule = import ./nix/module.nix;
}
