use abi_stable::{
    export_root_module,
    prefix_type::PrefixTypeTrait,
    std_types::{RResult, RStr, RString, RVec},
};
use chrono::DateTime;
use kkc_plugin_api::{
    KKC_REMOTE_PLUGIN_API_VERSION, RemoteConfigField, RemoteEntry, RemotePluginMetadata,
    RemotePluginMod, RemotePluginModRef, RemotePluginResult,
};
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use serde::Deserialize;
use serde_json::{Value, json};
use std::fs;
use std::io::Read;
use std::path::Path;
use std::sync::Mutex;

const GRAPH_ROOT: &str = "https://graph.microsoft.com/v1.0/me/drive/root";
const APP_KEY: &str = "b325403f-549c-4988-ba73-81d38df9ac4f";
const DEFAULT_SCOPES: &str = "offline_access Files.ReadWrite.All";

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
    tenant: Option<String>,
    #[serde(default)]
    scopes: Option<String>,
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
        id: "onedrive".into(),
        name: "OneDrive".into(),
        version: "0.1.1".into(),
        description: "OneDrive remote filesystem using Microsoft Graph".into(),
        scheme: "onedrive".into(),
        fields: vec![
            RemoteConfigField::new("app_key", "App key", false, true, APP_KEY),
            RemoteConfigField::new("refresh_token", "Refresh token", true, true, ""),
            RemoteConfigField::new("tenant", "Tenant", false, false, "common"),
            RemoteConfigField::new("scopes", "Scopes", false, false, DEFAULT_SCOPES),
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
        let url = graph_item_url(cwd.as_str(), "children");
        let response = api_get_json(&token, &url)?;
        let entries = response
            .get("value")
            .and_then(Value::as_array)
            .ok_or_else(|| "OneDrive children response has no value array".to_string())?;
        let mut out = Vec::new();
        for entry in entries {
            let name = entry.get("name").and_then(Value::as_str).unwrap_or("");
            if name.is_empty() || (!show_hidden && name.starts_with('.')) {
                continue;
            }
            let is_dir = entry.get("folder").is_some();
            let modified_unix = entry
                .get("lastModifiedDateTime")
                .and_then(Value::as_str)
                .and_then(parse_rfc3339_unix)
                .unwrap_or(0);
            let path = join_remote(cwd.as_str(), name);
            debug_log(&format!(
                "onedrive list_dir: cwd={}, name={}, joined_path={}",
                cwd.as_str(),
                name,
                &path
            ));
            out.push(RemoteEntry {
                name: name.into(),
                path: path.into(),
                is_dir,
                is_symlink: false,
                size: entry.get("size").and_then(Value::as_u64).unwrap_or(0),
                modified_unix,
                mode: if is_dir { 0o755 } else { 0o644 },
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
            return Err("OneDrive directory download is not implemented yet".to_string());
        }
        let cfg = parse_config(config_json.as_str())?;
        let token = access_token(&cfg)?;
        let remote = normalize_path(remote_path.as_str());
        let name = Path::new(&remote)
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| "remote path has no file name".to_string())?;
        let local = Path::new(local_dir.as_str()).join(name);
        let response = ureq::get(&graph_item_url(&remote, "content"))
            .set("Authorization", &format!("Bearer {token}"))
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
            return Err("OneDrive directory upload is not implemented yet".to_string());
        }
        let cfg = parse_config(config_json.as_str())?;
        let token = access_token(&cfg)?;
        let local = Path::new(local_path.as_str());
        let name = local
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| "local path has no file name".to_string())?;
        let remote = join_remote(remote_dir.as_str(), name);
        let mut bytes = Vec::new();
        fs::File::open(local)
            .map_err(|err| err.to_string())?
            .read_to_end(&mut bytes)
            .map_err(|err| err.to_string())?;
        ureq::put(&graph_item_url(&remote, "content"))
            .set("Authorization", &format!("Bearer {token}"))
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
        ureq::delete(&graph_item_url(remote_path.as_str(), ""))
            .set("Authorization", &format!("Bearer {token}"))
            .call()
            .map_err(http_error)?;
        Ok(())
    })
}

extern "C" fn make_dir(config_json: RStr<'_>, remote_path: RStr<'_>) -> RemotePluginResult<()> {
    wrap(|| {
        let cfg = parse_config(config_json.as_str())?;
        let token = access_token(&cfg)?;
        let remote = normalize_path(remote_path.as_str());
        let name = Path::new(&remote)
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| "remote path has no folder name".to_string())?;
        let parent = Path::new(&remote)
            .parent()
            .and_then(|parent| parent.to_str())
            .unwrap_or("/");
        api_post_json(
            &token,
            &graph_item_url(parent, "children"),
            json!({
                "name": name,
                "folder": {},
                "@microsoft.graph.conflictBehavior": "fail"
            }),
        )?;
        Ok(())
    })
}

extern "C" fn auth_start(config_json: RStr<'_>) -> RemotePluginResult<RString> {
    wrap(|| {
        let cfg = parse_partial_config(config_json.as_str())?;
        let client_id =
            onedrive_client_id(&cfg).ok_or_else(|| "OneDrive app_key is required".to_string())?;
        let tenant = optional_value(cfg.tenant.as_deref()).unwrap_or("common");
        let scopes = optional_value(cfg.scopes.as_deref()).unwrap_or(DEFAULT_SCOPES);
        let body = form_urlencoded(&[("client_id", client_id), ("scope", scopes)]);
        let response = ureq::post(&format!(
            "https://login.microsoftonline.com/{tenant}/oauth2/v2.0/devicecode"
        ))
        .set("Content-Type", "application/x-www-form-urlencoded")
        .send_string(&body)
        .map_err(http_error)?;
        let text = response.into_string().map_err(|err| err.to_string())?;
        let mut session = serde_json::from_str::<Value>(&text).map_err(|err| err.to_string())?;
        session["type"] = json!("device_code");
        session["tenant"] = json!(tenant);
        session["client_id"] = json!(client_id);
        session["scopes"] = json!(scopes);
        Ok(session.to_string().into())
    })
}

extern "C" fn auth_complete(
    _config_json: RStr<'_>,
    auth_session_json: RStr<'_>,
    _input: RStr<'_>,
) -> RemotePluginResult<RString> {
    wrap(|| {
        let session = serde_json::from_str::<Value>(auth_session_json.as_str())
            .map_err(|err| err.to_string())?;
        let device_code = session
            .get("device_code")
            .and_then(Value::as_str)
            .ok_or_else(|| "OneDrive auth session has no device_code".to_string())?;
        let client_id = session
            .get("client_id")
            .and_then(Value::as_str)
            .unwrap_or(APP_KEY);
        let tenant = session
            .get("tenant")
            .and_then(Value::as_str)
            .unwrap_or("common");
        let scopes = session
            .get("scopes")
            .and_then(Value::as_str)
            .unwrap_or(DEFAULT_SCOPES);
        let body = form_urlencoded(&[
            ("client_id", client_id),
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ("device_code", device_code),
        ]);
        let response = ureq::post(&format!(
            "https://login.microsoftonline.com/{tenant}/oauth2/v2.0/token"
        ))
        .set("Content-Type", "application/x-www-form-urlencoded")
        .send_string(&body)
        .map_err(http_error)?;
        let text = response.into_string().map_err(|err| err.to_string())?;
        let token_json = serde_json::from_str::<Value>(&text).map_err(|err| err.to_string())?;
        let refresh_token = token_json
            .get("refresh_token")
            .and_then(Value::as_str)
            .ok_or_else(|| "OneDrive token response has no refresh_token".to_string())?;
        Ok(json!({
            "app_key": client_id,
            "refresh_token": refresh_token,
            "tenant": tenant,
            "scopes": scopes,
        })
        .to_string()
        .into())
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
        return Err("OneDrive refresh_token is required".to_string());
    }
    if onedrive_client_id(&cfg).is_none() {
        return Err("OneDrive app_key is required".to_string());
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
        .ok_or_else(|| "OneDrive refresh_token is required".to_string())?;
    let client_id =
        onedrive_client_id(cfg).ok_or_else(|| "OneDrive app_key is required".to_string())?;
    let tenant = optional_value(cfg.tenant.as_deref()).unwrap_or("common");
    let scopes = optional_value(cfg.scopes.as_deref()).unwrap_or(DEFAULT_SCOPES);
    let body = form_urlencoded(&[
        ("client_id", client_id),
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("scope", scopes),
    ]);
    let response = ureq::post(&format!(
        "https://login.microsoftonline.com/{tenant}/oauth2/v2.0/token"
    ))
    .set("Content-Type", "application/x-www-form-urlencoded")
    .send_string(&body)
    .map_err(http_error)?;
    let text = response.into_string().map_err(|err| err.to_string())?;
    let json = serde_json::from_str::<Value>(&text).map_err(|err| err.to_string())?;
    json.get("access_token")
        .and_then(Value::as_str)
        .filter(|token| !token.trim().is_empty())
        .map(str::to_string)
        .ok_or_else(|| "OneDrive token response has no access_token".to_string())
}

fn api_get_json(token: &str, url: &str) -> Result<Value, String> {
    let response = ureq::get(url)
        .set("Authorization", &format!("Bearer {token}"))
        .call()
        .map_err(http_error)?;
    let text = response.into_string().map_err(|err| err.to_string())?;
    serde_json::from_str::<Value>(&text).map_err(|err| err.to_string())
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

fn onedrive_client_id(cfg: &Config) -> Option<&str> {
    optional_value(cfg.app_key.as_deref())
        .or_else(|| optional_value(cfg.client_id.as_deref()))
        .or(Some(APP_KEY))
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

fn http_error(err: ureq::Error) -> String {
    match err {
        ureq::Error::Status(code, response) => {
            let mut body = String::new();
            let _ = response.into_reader().read_to_string(&mut body);
            format!("OneDrive HTTP {code}: {body}")
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

fn graph_item_url(path: &str, suffix: &str) -> String {
    let normalized = normalize_path(path);
    let suffix = suffix.trim_matches('/');
    match (normalized.as_str(), suffix.is_empty()) {
        ("/", true) => GRAPH_ROOT.to_string(),
        ("/", false) => format!("{GRAPH_ROOT}/{suffix}"),
        (_, true) => format!("{GRAPH_ROOT}:{}:", encode_graph_path(&normalized)),
        (_, false) => format!("{GRAPH_ROOT}:{}:/{suffix}", encode_graph_path(&normalized)),
    }
}

fn encode_graph_path(path: &str) -> String {
    normalize_path(path)
        .split('/')
        .filter(|part| !part.is_empty())
        .map(|part| utf8_percent_encode(part, NON_ALPHANUMERIC).to_string())
        .fold(String::new(), |mut acc, part| {
            acc.push('/');
            acc.push_str(&part);
            acc
        })
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
