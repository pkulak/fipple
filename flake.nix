{
  description = "A PipeWire meeting recorder that transcribes and summarizes through OpenRouter";

  inputs.nixpkgs.url = "github:nixos/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      forAllSystems =
        f:
        nixpkgs.lib.genAttrs [ "x86_64-linux" "aarch64-linux" ] (
          system: f system nixpkgs.legacyPackages.${system}
        );
    in
    {
      packages = forAllSystems (
        system: pkgs: {
          default = pkgs.rustPlatform.buildRustPackage {
            pname = "fipple";
            version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).package.version;
            src = self;
            cargoLock.lockFile = ./Cargo.lock;
            # rustls's crypto provider links against it.
            buildInputs = [ pkgs.aws-lc ];
            nativeBuildInputs = [ pkgs.pkg-config ];
            # The tests generate audio fixtures and convert them to MP3, and the
            # HTTP client won't build without CA certificates.
            nativeCheckInputs = [
              pkgs.ffmpeg-headless
              pkgs.cacert
            ];
          };
        }
      );

      checks = forAllSystems (
        system: pkgs: {
          fipple = self.packages.${system}.default;
        }
      );

      devShells = forAllSystems (
        system: pkgs: {
          default = pkgs.mkShell {
            packages = with pkgs; [
              cargo
              rustc
              clippy
              rustfmt
              rust-analyzer
              ffmpeg-headless
            ];
          };
        }
      );
    };
}
