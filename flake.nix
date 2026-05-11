{
  description = "BlueVein with Nix package and NixOS module";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f system);
    in
    {
      packages = forAllSystems (system:
        let
          pkgs = import nixpkgs { inherit system; };
        in
        {
          default = pkgs.callPackage ./nix/package.nix { };
        });

      nixosModules.default = import ./nix/module.nix;

      formatter = forAllSystems (system:
        let
          pkgs = import nixpkgs { inherit system; };
        in
        pkgs.nixfmt);

      checks = forAllSystems (system:
        let
          pkgs = import nixpkgs { inherit system; };
          moduleEval = nixpkgs.lib.nixosSystem {
            inherit system;
            modules = [
              self.nixosModules.default
              {
                services.bluevein.enable = true;
                system.stateVersion = "25.05";
              }
            ];
          };
        in
        {
          package-build = self.packages.${system}.default;

          module-eval = pkgs.runCommand "bluevein-module-eval" { } ''
            test -n "${moduleEval.config.systemd.services.bluevein.serviceConfig.ExecStart}"
            touch "$out"
          '';

          rust-tests = pkgs.callPackage ./nix/package.nix { doCheck = true; };
        });
    };
}
