{
  lib,
  rustPlatform,
  pkg-config,
  sqlite,
}:
let
  fs = lib.fileset;
  src = fs.toSource {
    root = ../.;
    fileset = fs.unions [
      ../Cargo.toml
      ../Cargo.lock
      ../crates
      ../proptest
      ../difftest
    ];
  };
in
rustPlatform.buildRustPackage {
  pname = "fast-nix-gc";
  version = "0.1.0";
  inherit src;
  cargoLock = {
    lockFile = ../Cargo.lock;
    outputHashes = {
      "harmonia-file-nar-3.1.0" = "sha256-YklzRujFo5lvFsdLoedE6OL6OvSwNk/nfwlGxulyTS4=";
    };
  };
  nativeBuildInputs = [ pkg-config ];
  buildInputs = [ sqlite ];
  cargoBuildFlags = [
    "-p"
    "fast-nix-gc"
    "-p"
    "fast-nix-optimise"
  ];
  # Each test spawns a gc process with its own rayon pool; cap threads so
  # parallel tests don't exhaust the sandbox thread limit.
  env.RAYON_NUM_THREADS = 2;

  cargoTestFlags = [
    "-p"
    "fast-nix-gc"
    "-p"
    "fast-nix-common"
    "-p"
    "fast-nix-optimise"
  ];

  meta = {
    description = "Faster nix-collect-garbage and nix-store --optimise";
    homepage = "https://github.com/Mic92/fast-nix-gc";
    license = lib.licenses.mit;
    mainProgram = "fast-nix-gc";
  };
}
