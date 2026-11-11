{
  description = "llm-index: local, sandboxed document and code index for LLM agents";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/f4a03c008967841f2a0b3cb7c436fc6fc31671f6";
  # Used only by checks/modules.nix.
  inputs.home-manager = {
    url = "github:nix-community/home-manager/82ce401aa350502897c3f2e85f384d61cd3b0b6d";
    inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs =
    {
      self,
      nixpkgs,
      home-manager,
    }:
    let
      system = "x86_64-linux";
      pkgs = nixpkgs.legacyPackages.${system};
      evalPackage = pkgs.callPackage ./nix/package.nix { features = [ "eval" ]; };
      vmChecks = import ./nix/checks/vm.nix {
        inherit pkgs nixosModule;
        inherit (nixpkgs) lib;
        package = self.packages.${system}.default;
        src = nixpkgs.lib.fileset.toSource {
          root = ./.;
          fileset = ./tests;
        };
      };
      hmModule = import ./nix/hm-module.nix { inherit self; };
      nixosModule = import ./nix/nixos-module.nix { inherit self; };
    in
    {
      homeManagerModules.default = hmModule;
      nixosModules.default = nixosModule;

      packages.${system} = {
        default = pkgs.callPackage ./nix/package.nix { };
        eval = evalPackage;
      };

      devShells.${system}.default = pkgs.mkShell {
        packages = with pkgs; [
          cargo
          rustc
          clippy
          rustfmt
          pkg-config
          pdfium
          oniguruma
        ];
        RUSTONIG_SYSTEM_LIBONIG = true;
        LLM_INDEX_PDFIUM_LIB = "${pkgs.pdfium}/lib/libpdfium.so";
        LLM_INDEX_MODEL_DIR = "${self.packages.${system}.default.passthru.modelDir}";
      };

      checks.${system} = vmChecks // {
        fixture = pkgs.callPackage ./nix/checks/fixture.nix {
          inherit evalPackage;
          src = nixpkgs.lib.fileset.toSource {
            root = ./.;
            fileset = ./tests;
          };
        };
        closure = pkgs.callPackage ./nix/checks/closure.nix {
          package = self.packages.${system}.default;
        };
        modules = import ./nix/checks/modules.nix {
          inherit
            pkgs
            nixpkgs
            home-manager
            hmModule
            nixosModule
            ;
          inherit (nixpkgs) lib;
        };
      };
    };
}
