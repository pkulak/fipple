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
            buildInputs = [
              # rustls's crypto provider links against it.
              pkgs.aws-lc
              pkgs.webrtc-audio-processing
            ];
            nativeBuildInputs = [
              pkgs.pkg-config
              pkgs.makeWrapper
              pkgs.rustPlatform.bindgenHook
            ];
            # The tests generate audio fixtures and convert them to MP3, and the
            # HTTP client won't build without CA certificates.
            nativeCheckInputs = [
              pkgs.ffmpeg-headless
              pkgs.cacert
            ];
            postInstall = ''
              wrapProgram $out/bin/fipple --prefix PATH : ${
                pkgs.lib.makeBinPath [
                  pkgs.pipewire
                  pkgs.ffmpeg-headless
                ]
              }
            '';
            meta.mainProgram = "fipple";
          };
        }
      );

      homeManagerModules.default =
        {
          config,
          lib,
          pkgs,
          ...
        }:
        let
          package = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
          cfg = config.services.fipple;
        in
        {
          options.services.fipple = {
            enable = lib.mkEnableOption "the fipple meeting recorder";
            echoCancel = lib.mkEnableOption "cancelling the speakers' echo out of the mic track";
            onFinished = lib.mkOption {
              type = lib.types.nullOr lib.types.path;
              default = null;
              description = "A script to run in each meeting folder after it's processed, with the folder as its argument.";
            };
            summaryInstructions = lib.mkOption {
              type = lib.types.nullOr lib.types.path;
              default = null;
              description = "A file of instructions to add to the summary model's system prompt.";
            };
            titleInstructions = lib.mkOption {
              type = lib.types.nullOr lib.types.path;
              default = null;
              description = "A file of instructions to add to the title model's system prompt.";
            };
          };

          config = lib.mkIf cfg.enable {
            home.packages = [ package ];
            systemd.user.services.fipple = {
              Unit.Description = "fipple meeting recorder";
              Service = {
                ExecStart = "${lib.getExe package} daemon";
                Restart = "on-failure";
                Environment =
                  lib.optional cfg.echoCancel "FIPPLE_ECHO_CANCEL=true"
                  ++ lib.optional (cfg.onFinished != null) "FIPPLE_ON_FINISHED=${cfg.onFinished}"
                  ++ lib.optional (
                    cfg.summaryInstructions != null
                  ) "FIPPLE_SUMMARY_INSTRUCTIONS=${cfg.summaryInstructions}"
                  ++ lib.optional (
                    cfg.titleInstructions != null
                  ) "FIPPLE_TITLE_INSTRUCTIONS=${cfg.titleInstructions}";
              };
              Install.WantedBy = [ "default.target" ];
            };
          };
        };

      checks = forAllSystems (
        system: pkgs: {
          fipple = self.packages.${system}.default;
        }
      );

      devShells = forAllSystems (
        system: pkgs: {
          default = pkgs.mkShell {
            inputsFrom = [ self.packages.${system}.default ];
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
