# kkc-plugins-rust

Rust native plugins for `kkc-rust`.

## Current plugins

- `dropbox` (`plugins/dropbox`)
- `onedrive` (`plugins/onedrive`)

## Local build

```bash
cargo build --release -p kkc-remote-dropbox
cargo build --release -p kkc-remote-onedrive
```

The runtime manifest for each plugin stays in its plugin directory (`plugin.toml`).
