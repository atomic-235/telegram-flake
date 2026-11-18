{
  lib,
  pkgs,
  ...
}:

# Built by CI, not by consumers: the resulting binary is published as the
# release asset that package.nix fetches.
#
# pkgsStatic gives a fully static musl binary. glibc static-pie is not usable
# here: with `cargo --target <host triple>` the target-specific rustflags also
# apply to build scripts and proc-macros, which cannot be linked static-pie.
let
  data = builtins.fromJSON (builtins.readFile ./version.json);
in
pkgs.pkgsStatic.rustPlatform.buildRustPackage {
  pname = "telegram";
  version = data.version;

  src = lib.cleanSourceWith {
    src = ./.;
    filter = path: _: baseNameOf path != "target";
  };
  cargoLock.lockFile = ./Cargo.lock;

  doCheck = false;

  meta = with lib; {
    description = "Telegram user-account CLI: read, search, send, digest";
    homepage = "https://github.com/atomic-235/telegram-flake";
    license = licenses.mit;
    platforms = [ "x86_64-linux" ];
    mainProgram = "telegram";
  };
}
