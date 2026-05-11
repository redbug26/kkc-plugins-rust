# kkc-plugins-rust

Rust native plugins for `kkc-rust`.

## Current plugins

- `dropbox` (`plugins/dropbox`)
- `onedrive` (`plugins/onedrive`)
- `ayt` (`plugins/ayt`)
- `xlsx` (`plugins/xlsx`)

## Local build

```bash
cargo build --release -p kkc-remote-dropbox
cargo build --release -p kkc-remote-onedrive
cargo build --release -p kkc-viewer-ayt
cargo build --release -p kkc-viewer-xlsx
```

`ayt` decodes AYT metadata in the plugin, while actual audio output is handled by `kkc-rust` core.

The runtime manifest for each plugin stays in its plugin directory (`plugin.toml`).
