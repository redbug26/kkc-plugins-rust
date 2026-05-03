use abi_stable::{
    export_root_module,
    prefix_type::PrefixTypeTrait,
    std_types::{RResult, RStr, RString, RVec},
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::DateTime;
use kkc_plugin_api::{
    KKC_REMOTE_PLUGIN_API_VERSION, RemoteConfigField, RemoteEntry, RemotePluginMetadata,
    RemotePluginMod, RemotePluginModRef, RemotePluginResult,
};
use percent_encoding::{NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Read;
use std::path::Path;
use std::sync::Mutex;

#[derive(Debug, Deserialize)]
struct Config {
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    app_key: Option<String>,
    #[serde(default)]
    client_id: Option<String>,
    #[serde(default)]
    app_secret: Option<String>,
    #[serde(default)]
    redirect_uri: Option<String>,
}

static DEBUG_LOGGER: Mutex<Option<usize>> = Mutex::new(None);

#[export_root_module]
pub fn get_library() -> RemotePluginModRef {
    RemotePluginMod {
        api_version,
        metadata,
        normalize_cwd,
        list_dir,
        download_into_dir,
        upload_into_dir,
        delete_path,
        set_debug_log,
        make_dir,
        auth_start,
        auth_complete,
    }
    .leak_into_prefix()
}

extern "C" fn api_version() -> u32 {
    KKC_REMOTE_PLUGIN_API_VERSION
}

extern "C" fn metadata() -> RemotePluginMetadata {
    RemotePluginMetadata {
        id: "dropbox".into(),
        name: "Dropbox".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        description: "Dropbox remote filesystem".into(),
        scheme: "dropbox".into(),
        fields: vec![
            RemoteConfigField::new("app_key", "App key", false, true, ""),
            RemoteConfigField::new("refresh_token", "Refresh token", true, true, ""),
            RemoteConfigField::new("app_secret", "App secret", true, false, ""),
            RemoteConfigField::new("redirect_uri", "Redirect URI", false, false, ""),
            RemoteConfigField::new("access_token", "Access token (legacy)", true, false, ""),
        ]
        .into(),
    }
}

extern "C" fn normalize_cwd(_config_json: RStr<'_>, cwd: RStr<'_>) -> RemotePluginResult<RString> {
    RResult::ROk(normalize_path(cwd.as_str()).into())
}

extern "C" fn set_debug_log(callback: usize) {
    if let Ok(mut slot) = DEBUG_LOGGER.lock() {
        *slot = Some(callback);
    }
}

extern "C" fn list_dir(
    config_json: RStr<'_>,
    cwd: RStr<'_>,
    show_hidden: bool,
) -> RemotePluginResult<RVec<RemoteEntry>> {
    wrap(|| {
        let cfg = parse_config(config_json.as_str())?;
        let token = access_token(&cfg)?;
        let path = dropbox_path(cwd.as_str());
        let body = json!({ "path": path });
        let response = api_post_json(
            &token,
            "https://api.dropboxapi.com/2/files/list_folder",
            body,
        )?;
        let entries = response
            .get("entries")
            .and_then(Value::as_array)
            .ok_or_else(|| "Dropbox list_folder response has no entries".to_string())?;
        let mut out = Vec::new();
        for entry in entries {
            let name = entry.get("name").and_then(Value::as_str).unwrap_or("");
            if name.is_empty() || (!show_hidden && name.starts_with('.')) {
                continue;
            }
            let tag = entry.get(".tag").and_then(Value::as_str).unwrap_or("file");
            let modified_unix = entry
                .get("server_modified")
                .and_then(Value::as_str)
                .and_then(parse_rfc3339_unix)
                .or_else(|| {
                    entry
                        .get("client_modified")
                        .and_then(Value::as_str)
                        .and_then(parse_rfc3339_unix)
                })
                .unwrap_or(0);
            // Build path by joining parent directory with entry name
            // This ensures correct paths even for special/shared folders
            let path = join_remote(cwd.as_str(), name);
            debug_log(&format!(
                "dropbox list_dir: cwd={}, name={}, joined_path={}",
                cwd.as_str(),
                name,
                &path
            ));
            out.push(RemoteEntry {
                name: name.into(),
                path: path.into(),
                is_dir: tag == "folder",
                is_symlink: false,
                size: entry.get("size").and_then(Value::as_u64).unwrap_or(0),
                modified_unix,
                mode: if tag == "folder" { 0o755 } else { 0o644 },
            });
        }
        out.sort_by(|a, b| a.name.as_str().cmp(b.name.as_str()));
        Ok(out.into())
    })
}

extern "C" fn download_into_dir(
    config_json: RStr<'_>,
    remote_path: RStr<'_>,
    local_dir: RStr<'_>,
    recursive: bool,
) -> RemotePluginResult<RString> {
    wrap(|| {
        if recursive {
            return Err("Dropbox directory download is not implemented yet".to_string());
        }
        let cfg = parse_config(config_json.as_str())?;
        let token = access_token(&cfg)?;
        let remote = normalize_path(remote_path.as_str());
        let name = Path::new(&remote)
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| "remote path has no file name".to_string())?;
        let local = Path::new(local_dir.as_str()).join(name);
        let arg = json!({ "path": dropbox_path(&remote) }).to_string();
        let response = ureq::post("https://content.dropboxapi.com/2/files/download")
            .set("Authorization", &format!("Bearer {token}"))
            .set("Dropbox-API-Arg", &arg)
            .call()
            .map_err(http_error)?;
        if let Some(parent) = local.parent() {
            fs::create_dir_all(parent).map_err(|err| err.to_string())?;
        }
        let mut file = fs::File::create(&local).map_err(|err| err.to_string())?;
        let mut reader = response.into_reader();
        std::io::copy(&mut reader, &mut file).map_err(|err| err.to_string())?;
        Ok(local.to_string_lossy().to_string().into())
    })
}

extern "C" fn upload_into_dir(
    config_json: RStr<'_>,
    local_path: RStr<'_>,
    remote_dir: RStr<'_>,
    recursive: bool,
) -> RemotePluginResult<RString> {
    wrap(|| {
        if recursive {
            return Err("Dropbox directory upload is not implemented yet".to_string());
        }
        let cfg = parse_config(config_json.as_str())?;
        let token = access_token(&cfg)?;
        let local = Path::new(local_path.as_str());
        let name = local
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| "local path has no file name".to_string())?;
        let remote = join_remote(remote_dir.as_str(), name);
        let arg = json!({
            "path": dropbox_path(&remote),
            "mode": "add",
            "autorename": true,
            "mute": false,
            "strict_conflict": false
        })
        .to_string();
        let mut bytes = Vec::new();
        fs::File::open(local)
            .map_err(|err| err.to_string())?
            .read_to_end(&mut bytes)
            .map_err(|err| err.to_string())?;
        ureq::post("https://content.dropboxapi.com/2/files/upload")
            .set("Authorization", &format!("Bearer {token}"))
            .set("Dropbox-API-Arg", &arg)
            .set("Content-Type", "application/octet-stream")
            .send_bytes(&bytes)
            .map_err(http_error)?;
        Ok(remote.into())
    })
}

extern "C" fn delete_path(
    config_json: RStr<'_>,
    remote_path: RStr<'_>,
    _is_dir: bool,
) -> RemotePluginResult<()> {
    wrap(|| {
        let cfg = parse_config(config_json.as_str())?;
        let token = access_token(&cfg)?;
        api_post_json(
            &token,
            "https://api.dropboxapi.com/2/files/delete_v2",
            json!({ "path": dropbox_path(remote_path.as_str()) }),
        )?;
        Ok(())
    })
}

extern "C" fn make_dir(config_json: RStr<'_>, remote_path: RStr<'_>) -> RemotePluginResult<()> {
    wrap(|| {
        let cfg = parse_config(config_json.as_str())?;
        let token = access_token(&cfg)?;
        api_post_json(
            &token,
            "https://api.dropboxapi.com/2/files/create_folder_v2",
            json!({ "path": dropbox_path(remote_path.as_str()), "autorename": false }),
        )?;
        Ok(())
    })
}

extern "C" fn auth_start(config_json: RStr<'_>) -> RemotePluginResult<RString> {
    wrap(|| {
        let cfg = parse_partial_config(config_json.as_str())?;
        let app_key =
            dropbox_client_id(&cfg).ok_or_else(|| "Dropbox app_key is required".to_string())?;
        let code_verifier = pkce_code_verifier()?;
        let code_challenge = pkce_code_challenge(&code_verifier);
        let mut params = vec![
            ("client_id", app_key),
            ("response_type", "code"),
            ("token_access_type", "offline"),
            ("code_challenge", code_challenge.as_str()),
            ("code_challenge_method", "S256"),
        ];
        if let Some(redirect_uri) = optional_value(cfg.redirect_uri.as_deref()) {
            params.push(("redirect_uri", redirect_uri));
        }
        let auth_url = format!(
            "https://www.dropbox.com/oauth2/authorize?{}",
            form_urlencoded(&params)
        );
        Ok(json!({
            "type": "authorization_code_pkce",
            "auth_url": auth_url,
            "instructions": "Open auth_url, authorize Dropbox, then paste the returned code into auth_complete input.",
            "code_verifier": code_verifier,
            "redirect_uri": optional_value(cfg.redirect_uri.as_deref()),
        })
        .to_string()
        .into())
    })
}

extern "C" fn auth_complete(
    config_json: RStr<'_>,
    auth_session_json: RStr<'_>,
    input: RStr<'_>,
) -> RemotePluginResult<RString> {
    wrap(|| {
        let cfg = parse_partial_config(config_json.as_str())?;
        let app_key =
            dropbox_client_id(&cfg).ok_or_else(|| "Dropbox app_key is required".to_string())?;
        let session = serde_json::from_str::<Value>(auth_session_json.as_str())
            .map_err(|err| err.to_string())?;
        let code_verifier = session
            .get("code_verifier")
            .and_then(Value::as_str)
            .ok_or_else(|| "Dropbox auth session has no code_verifier".to_string())?;
        let code = extract_oauth_code(input.as_str())?;
        let mut form = vec![
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("client_id", app_key),
            ("code_verifier", code_verifier),
        ];
        if let Some(redirect_uri) = optional_value(cfg.redirect_uri.as_deref()) {
            form.push(("redirect_uri", redirect_uri));
        }
        let body = form_urlencoded(&form);
        let response = ureq::post("https://api.dropbox.com/oauth2/token")
            .set("Content-Type", "application/x-www-form-urlencoded")
            .send_string(&body)
            .map_err(http_error)?;
        let text = response.into_string().map_err(|err| err.to_string())?;
        let token_json = serde_json::from_str::<Value>(&text).map_err(|err| err.to_string())?;
        let refresh_token = token_json
            .get("refresh_token")
            .and_then(Value::as_str)
            .ok_or_else(|| "Dropbox token response has no refresh_token".to_string())?;
        let mut out = json!({
            "app_key": app_key,
            "refresh_token": refresh_token,
        });
        if let Some(redirect_uri) = optional_value(cfg.redirect_uri.as_deref()) {
            out["redirect_uri"] = json!(redirect_uri);
        }
        Ok(out.to_string().into())
    })
}

fn wrap<T>(f: impl FnOnce() -> Result<T, String>) -> RemotePluginResult<T> {
    match f() {
        Ok(value) => RResult::ROk(value),
        Err(err) => RResult::RErr(err.into()),
    }
}

fn parse_config(raw: &str) -> Result<Config, String> {
    let cfg = parse_partial_config(raw)?;
    if optional_value(cfg.access_token.as_deref()).is_some() {
        return Ok(cfg);
    }
    if optional_value(cfg.refresh_token.as_deref()).is_none() {
        return Err("Dropbox refresh_token is required".to_string());
    }
    if dropbox_client_id(&cfg).is_none() {
        return Err("Dropbox app_key is required".to_string());
    }
    Ok(cfg)
}

fn parse_partial_config(raw: &str) -> Result<Config, String> {
    if raw.trim().is_empty() {
        serde_json::from_str("{}").map_err(|err| err.to_string())
    } else {
        serde_json::from_str(raw).map_err(|err| err.to_string())
    }
}

fn access_token(cfg: &Config) -> Result<String, String> {
    if let Some(token) = optional_value(cfg.access_token.as_deref()) {
        return Ok(token.to_string());
    }
    refresh_access_token(cfg)
}

fn refresh_access_token(cfg: &Config) -> Result<String, String> {
    let refresh_token = optional_value(cfg.refresh_token.as_deref())
        .ok_or_else(|| "Dropbox refresh_token is required".to_string())?;
    let app_key =
        dropbox_client_id(cfg).ok_or_else(|| "Dropbox app_key is required".to_string())?;
    let mut form = vec![
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", app_key),
    ];
    if let Some(secret) = optional_value(cfg.app_secret.as_deref()) {
        form.push(("client_secret", secret));
    }
    let body = form_urlencoded(&form);
    let response = ureq::post("https://api.dropbox.com/oauth2/token")
        .set("Content-Type", "application/x-www-form-urlencoded")
        .send_string(&body)
        .map_err(http_error)?;
    let text = response.into_string().map_err(|err| err.to_string())?;
    let json = serde_json::from_str::<Value>(&text).map_err(|err| err.to_string())?;
    json.get("access_token")
        .and_then(Value::as_str)
        .filter(|token| !token.trim().is_empty())
        .map(str::to_string)
        .ok_or_else(|| "Dropbox token response has no access_token".to_string())
}

fn api_post_json(token: &str, url: &str, body: Value) -> Result<Value, String> {
    let response = ureq::post(url)
        .set("Authorization", &format!("Bearer {token}"))
        .set("Content-Type", "application/json")
        .send_string(&body.to_string())
        .map_err(http_error)?;
    let text = response.into_string().map_err(|err| err.to_string())?;
    serde_json::from_str::<Value>(&text).map_err(|err| err.to_string())
}

fn dropbox_client_id(cfg: &Config) -> Option<&str> {
    optional_value(cfg.app_key.as_deref()).or_else(|| optional_value(cfg.client_id.as_deref()))
}

fn optional_value(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn form_urlencoded(fields: &[(&str, &str)]) -> String {
    fields
        .iter()
        .map(|(key, value)| {
            format!(
                "{}={}",
                utf8_percent_encode(key, NON_ALPHANUMERIC),
                utf8_percent_encode(value, NON_ALPHANUMERIC)
            )
        })
        .collect::<Vec<_>>()
        .join("&")
}

fn pkce_code_verifier() -> Result<String, String> {
    let mut bytes = [0_u8; 32];
    getrandom::getrandom(&mut bytes).map_err(|err| err.to_string())?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn pkce_code_challenge(code_verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(code_verifier.as_bytes()))
}

fn extract_oauth_code(input: &str) -> Result<String, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("OAuth code is required".to_string());
    }
    if let Some(query_start) = trimmed.find('?') {
        let query = &trimmed[query_start + 1..];
        for pair in query.split('&') {
            let mut parts = pair.splitn(2, '=');
            let key = parts.next().unwrap_or("");
            let value = parts.next().unwrap_or("");
            if key == "code" {
                return percent_decode_str(value)
                    .decode_utf8()
                    .map(|value| value.to_string())
                    .map_err(|err| err.to_string());
            }
        }
    }
    Ok(trimmed.to_string())
}

fn http_error(err: ureq::Error) -> String {
    match err {
        ureq::Error::Status(code, response) => {
            let mut body = String::new();
            let _ = response.into_reader().read_to_string(&mut body);
            format!("Dropbox HTTP {code}: {body}")
        }
        ureq::Error::Transport(err) => err.to_string(),
    }
}

fn normalize_path(path: &str) -> String {
    let trimmed = path.trim();
    if trimmed.is_empty() || trimmed == "/" {
        "/".to_string()
    } else {
        format!("/{}", trimmed.trim_matches('/'))
    }
}

fn dropbox_path(path: &str) -> String {
    let normalized = normalize_path(path);
    if normalized == "/" {
        String::new()
    } else {
        normalized
    }
}

fn join_remote(parent: &str, name: &str) -> String {
    let parent = normalize_path(parent);
    if parent == "/" {
        format!("/{name}")
    } else {
        format!("{}/{}", parent.trim_end_matches('/'), name)
    }
}

fn parse_rfc3339_unix(value: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|dt| dt.timestamp())
}

fn debug_log(message: &str) {
    if let Ok(slot) = DEBUG_LOGGER.lock()
        && let Some(cb) = *slot
    {
        let callback: extern "C" fn(RString) = unsafe { std::mem::transmute(cb) };
        callback(message.into());
    }
}
