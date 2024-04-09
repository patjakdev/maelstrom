{
  lib,
  stdenv,
  craneLib,
  binaryen,
  pkg-config,
  llvmPackages,
  openssl,
  libiconv,
}:

let
  inherit (craneLib) buildPackage filterCargoSources path;
  inherit (lib) cleanSourceWith optionals;
  inherit (lib.strings) match;

  # Only keeps markdown files
  tarFilter = path: _type: match ".*tar$" path != null;
  tarOrCargo = path: type: (tarFilter path type) || (filterCargoSources path type);

  src = cleanSourceWith {
    src = path ./.;
    filter = tarOrCargo;
  };
in

buildPackage {
  # NOTE: we need to force lld otherwise rust-lld is not found for wasm32 target
  env.CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_LINKER = "lld";

  pname = "all";
  inherit src;

  strictDeps = true;

  nativeBuildInputs = [
    binaryen
    pkg-config
    llvmPackages.bintools
  ];

  buildInputs = [ openssl ] ++ optionals stdenv.isDarwin [ libiconv ];

  doCheck = false;
}
