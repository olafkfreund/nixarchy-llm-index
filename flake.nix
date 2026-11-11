{
  description = "llm-index: local, sandboxed document and code index for LLM agents";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/f4a03c008967841f2a0b3cb7c436fc6fc31671f6";

  outputs =
    { self, nixpkgs }:
    let
      system = "x86_64-linux";
      pkgs = nixpkgs.legacyPackages.${system};
    in
    {
      packages.${system}.default = pkgs.callPackage ./nix/package.nix { };

      devShells.${system}.default = pkgs.mkShell {
        packages = with pkgs; [
          cargo
          rustc
          clippy
          rustfmt
          pkg-config
          pdfium
        ];
      };

      checks.${system}.closure = pkgs.callPackage ./nix/checks/closure.nix {
        package = self.packages.${system}.default;
      };
    };
}
