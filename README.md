# kkc-plugins-rust

Rust native plugins for `kkc-rust`.

## Current plugins

- `dropbox` (`plugins/dropbox`)

## Local build

```bash
cargo build --release -p kkc-remote-dropbox
```

The runtime manifest for each plugin stays in its plugin directory (`plugin.toml`).
