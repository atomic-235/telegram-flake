{
  lib,
  stdenvNoCC,
  fetchurl,
}:

let
  data = builtins.fromJSON (builtins.readFile ./version.json);
  system = stdenvNoCC.hostPlatform.system;

  asset =
    {
      x86_64-linux = "telegram-linux-x86_64";
    }
    .${system} or (throw "telegram: unsupported system ${system}");
in
stdenvNoCC.mkDerivation {
  pname = "telegram";
  version = data.version;

  # Static musl binary with zero /nix/store references, so a plain fixed-output
  # fetch is enough.
  src = fetchurl {
    url = "https://github.com/atomic-235/telegram-flake/releases/download/v${data.version}/${asset}";
    hash = data.assets.${system};
  };

  dontUnpack = true;

  installPhase = ''
    runHook preInstall
    install -Dm755 "$src" "$out/bin/telegram"
    runHook postInstall
  '';

  meta = with lib; {
    description = "Telegram user-account CLI: read, search, send, digest (static release binary)";
    homepage = "https://github.com/atomic-235/telegram-flake";
    license = licenses.mit;
    platforms = [ "x86_64-linux" ];
    mainProgram = "telegram";
  };
}
