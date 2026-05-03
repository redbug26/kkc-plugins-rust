# OneDrive remote plugin

Native Rust remote plugin for `kkc-rust`, loaded through `abi_stable`.

Build from this directory:

```bash
env CARGO_TARGET_DIR=target cargo build --release
```

The runtime manifest points at `target/release/libkkc_remote_onedrive.dylib`.
On Linux or Windows, update `plugin.toml` to the platform library filename after building.

Configuration is passed to the plugin as JSON. Prefer a refresh-token based Microsoft Graph OAuth configuration:

```json
{
  "app_key": "b325403f-549c-4988-ba73-81d38df9ac4f",
  "refresh_token": "MS_GRAPH_OAUTH_REFRESH_TOKEN",
  "tenant": "common",
  "scopes": "offline_access Files.ReadWrite.All"
}
```

The plugin refreshes short-lived Microsoft Graph access tokens automatically. Existing configurations that pass `"access_token"` still work for compatibility.

With ABI v3, callers can create this configuration through the plugin auth flow:

1. Call `auth_start`; the plugin starts a Microsoft device-code flow.
2. Show the returned `verification_uri` or `verification_uri_complete` and `user_code` to the user.
3. After the user authorizes, call `auth_complete`.
4. Store the returned JSON as the profile `config_json`.

The plugin uses Microsoft Graph `/me/drive/root` APIs for listing, downloading, uploading, deleting, and creating folders.
