# Dropbox remote plugin

Native Rust remote plugin for `kkc-rust`, loaded through `abi_stable`.

Build from this directory:

```bash
env CARGO_TARGET_DIR=target cargo build --release
```

The runtime manifest points at `target/release/libkkc_remote_dropbox.dylib`.
On Linux or Windows, update `plugin.toml` to the platform library filename after building.

Configuration is passed to the plugin as JSON. The current Dropbox implementation expects:

```json
{ "access_token": "DROPBOX_OAUTH_ACCESS_TOKEN" }
```
