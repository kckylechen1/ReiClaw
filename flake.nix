{
  inputs = {
    flake-utils.url = "github:numtide/flake-utils";
    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    nixpkgs.url = "nixpkgs/nixos-unstable";
  };

  outputs = { flake-utils, fenix, nixpkgs, ... }:
    let
      nixosModule = { pkgs, ... }: {
        nixpkgs.overlays = [ fenix.overlays.default ];
        environment.systemPackages = [
          (pkgs.fenix.stable.withComponents [
            "cargo"
            "clippy"
            "rust-src"
            "rustc"
            "rustfmt"
          ])
          pkgs.rust-analyzer
        ];
      };
    in
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ fenix.overlays.default ];
        };
        rustToolchain = pkgs.fenix.stable.withComponents [
          "cargo"
          "clippy"
          "rust-src"
          "rustc"
          "rustfmt"
        ];
        nixosModuleEvalTests = import ./nix/eval-tests.nix {
          inherit nixpkgs system;
        };
        # >>> generated:flake-packages by `cargo generate installers` - do not edit <<<
        # Default feature set: canonical lean Dist.
        # Override with `packages.reiclaw.override { features = [ ... ]; }`.
        reiclawDefaultFeatures = [ "agent-runtime" "channel-acp-server" "channel-telegram" "channel-webhook" "gateway" "memory-core" "observability-prometheus" "schema-export" ];
        rootFeatureGraph = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).features;
        rootFeatureClosure = features: builtins.map (node: node.key) (builtins.genericClosure {
          startSet = builtins.map (feature: { key = feature; }) features;
          operator = node: builtins.map (feature: { key = feature; })
            (builtins.filter (feature: builtins.hasAttr feature rootFeatureGraph)
              (rootFeatureGraph.${node.key} or []));
        });
        buildReiclaw = pkgs.lib.makeOverridable
          ({ pname, cargoPkg, features ? reiclawDefaultFeatures }:
            let
              withAcpBridge = builtins.elem "agent-runtime" (rootFeatureClosure features);
            in
            (pkgs.makeRustPlatform {
              cargo = rustToolchain;
              rustc = rustToolchain;
            }).buildRustPackage {
              inherit pname;
              version = "0.8.3";
              src = ./.;
              cargoLock = {
                lockFile = ./Cargo.lock;
                outputHashes = builtins.fromJSON (builtins.readFile ./nix/hashes.json);
              };
              cargoBuildFlags =
                [ "-p" cargoPkg "--no-default-features" ]
                ++ pkgs.lib.optionals withAcpBridge [ "-p" "reiclaw-bridge-acp" ]
                ++ pkgs.lib.optionals (features != [])
                  [ "--features" (pkgs.lib.concatStringsSep ","
                    (builtins.map (feature: "${cargoPkg}/${feature}") features)) ];
              doCheck = false;
              buildInputs = [ pkgs.stdenv.cc.cc ];
              postInstall = ''
                test -x "$out/bin/rei"
              '' + pkgs.lib.optionalString withAcpBridge ''
                test -x "$out/bin/rei-acp-bridge"
              '';
            });
        # >>> end generated:flake-packages <<<
      in let
        reiclawPkg = buildReiclaw { pname = "reiclaw"; cargoPkg = "reiclaw"; };
      in {
        packages.reiclaw = reiclawPkg;
        packages.default = reiclawPkg;
        checks = pkgs.lib.optionalAttrs pkgs.stdenv.isLinux {
          nixos-module-eval = pkgs.writeText "reiclaw-nixos-module-eval" (
            builtins.toJSON nixosModuleEvalTests
          );
        };
        devShells.default = pkgs.mkShell {
          packages = [
            rustToolchain
            pkgs.rust-analyzer
            pkgs.nix-prefetch-git
            pkgs.jq
          ];
        };
      }) // {
      # The `services.reiclaw` NixOS module (multi-instance; see nix/module.nix
      # and nix/README.md). Exposed as the default so `nixosModules.default` can
      # be imported directly into a system configuration.
      nixosModules.default = import ./nix/module.nix;

      # Toolchain test systems used to evaluate the dev-shell module on both
      # supported Linux architectures; not a deployment target. The `checks`
      # output evaluates these in CI.
      nixosConfigurations = {
        nixos = nixpkgs.lib.nixosSystem {
          system = "x86_64-linux";
          modules = [ nixosModule ];
        };

        nixos-aarch64 = nixpkgs.lib.nixosSystem {
          system = "aarch64-linux";
          modules = [ nixosModule ];
        };
      };
    };
}
