{ pkgs ? import <nixpkgs> { } }:
with pkgs;
mkShell rec {
  nativeBuildInputs = [
    pkg-config rustup
  ];
  buildInputs = [
  ];
  RUSTC_VERSION =
    builtins.elemAt
      (builtins.match
        ".*channel *= *\"([^\"]*)\".*"
        (pkgs.lib.readFile ./rust-toolchain.toml)
      )
      0;
  LD_LIBRARY_PATH = lib.makeLibraryPath buildInputs;
  shellHook = ''
    rustup toolchain install ''${RUSTC_VERSION}
  '';
}
