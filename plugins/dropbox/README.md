# Dropbox remote plugin

Native Rust remote plugin for `kkc-rust`, loaded through `abi_stable`.

Build from this directory:

```bash
env CARGO_TARGET_DIR=target cargo build --release
```

The runtime manifest points at `target/release/libkkc_remote_dropbox.dylib`.
On Linux or Windows, update `plugin.toml` to the platform library filename after building.

Configuration is passed to the plugin as JSON. Prefer a refresh-token based OAuth configuration:

```json
{
  "app_key": "DROPBOX_APP_KEY",
  "refresh_token": "DROPBOX_OAUTH_REFRESH_TOKEN"
}
```

The plugin refreshes short-lived Dropbox access tokens automatically. Existing configurations that pass `"access_token"` still work for compatibility.

With ABI v3, callers can create this configuration through the plugin auth flow:

1. Call `auth_start` with at least `{ "app_key": "DROPBOX_APP_KEY" }`.
2. Open the returned `auth_url`.
3. Pass the returned authorization code to `auth_complete`.
4. Store the returned JSON as the profile `config_json`.
