# telegram-flake

A Telegram user-account CLI, built once by CI and published as a static
`x86_64-linux` binary. Consumers fetch the release asset; nobody compiles Rust
on `nixos-rebuild`.

Subcommands: `me`, `chats`, `messages`, `search`, `send`, `digest`, `batch`,
`download`, `contrarian-news`.

## Secrets

The binary reads its secrets as JSON on **stdin** and never from the
environment or argv:

```bash
sops decrypt --output-type json secrets/telegram.yaml | telegram me
```

Expected keys: `TELEGRAM_API_ID`, `TELEGRAM_API_HASH`, `TELEGRAM_SESSION_STRING`
(Telethon `StringSession` format, imported directly).

## Channel map

`contrarian-news` takes its source list from a JSON file of
`{"<channel id>": "<display name>"}`, defaulting to
`$XDG_CONFIG_HOME/telegram/channels.json` (`~/.config/telegram/channels.json`).
`--channels PATH` or `TELEGRAM_CHANNELS` overrides it. Nothing about which
channels are watched is compiled into the binary.

## Proxy

A SOCKS5 proxy on `127.0.0.1:12334` is used when it is listening;
`TG_PROXY=socks5://host:port` overrides it, `TG_PROXY=none` forces a direct
connection.

## Install

```nix
{
  inputs.telegram-flake.url = "github:atomic-235/telegram-flake";
}
```

```nix
{ inputs, pkgs, ... }:
{
  home.packages = [ inputs.telegram-flake.packages.${pkgs.system}.default ];
}
```

## Known gaps

- `topics` is a stub: the TL layer has no forum-topic types.
- `relogin` is not implemented; sessions are created elsewhere and pasted into
  the sops document.
