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
use serde::Deserialize;
use serde_json::{Value, json};
use std::fs;
use std::io::Read;
use std::path::Path;
use std::sync::Mutex;

#[derive(Debug, Deserialize)]
struct Config {
    access_token: String,
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
        version: "0.1.0".into(),
        description: "Dropbox remote filesystem".into(),
        scheme: "dropbox".into(),
        fields: vec![RemoteConfigField::new(
            "access_token",
            "Access token",
            true,
            true,
            "",
        )]
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
        let path = dropbox_path(cwd.as_str());
        let body = json!({ "path": path });
        let response = api_post_json(&cfg, "https://api.dropboxapi.com/2/files/list_folder", body)?;
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
        let remote = normalize_path(remote_path.as_str());
        let name = Path::new(&remote)
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| "remote path has no file name".to_string())?;
        let local = Path::new(local_dir.as_str()).join(name);
        let arg = json!({ "path": dropbox_path(&remote) }).to_string();
        let response = ureq::post("https://content.dropboxapi.com/2/files/download")
            .set("Authorization", &format!("Bearer {}", cfg.access_token))
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
            .set("Authorization", &format!("Bearer {}", cfg.access_token))
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
        api_post_json(
            &cfg,
            "https://api.dropboxapi.com/2/files/delete_v2",
            json!({ "path": dropbox_path(remote_path.as_str()) }),
        )?;
        Ok(())
    })
}

extern "C" fn make_dir(config_json: RStr<'_>, remote_path: RStr<'_>) -> RemotePluginResult<()> {
    wrap(|| {
        let cfg = parse_config(config_json.as_str())?;
        api_post_json(
            &cfg,
            "https://api.dropboxapi.com/2/files/create_folder_v2",
            json!({ "path": dropbox_path(remote_path.as_str()), "autorename": false }),
        )?;
        Ok(())
    })
}

fn wrap<T>(f: impl FnOnce() -> Result<T, String>) -> RemotePluginResult<T> {
    match f() {
        Ok(value) => RResult::ROk(value),
        Err(err) => RResult::RErr(err.into()),
    }
}

fn parse_config(raw: &str) -> Result<Config, String> {
    let cfg: Config = serde_json::from_str(raw).map_err(|err| err.to_string())?;
    if cfg.access_token.trim().is_empty() {
        return Err("Dropbox access_token is required".to_string());
    }
    Ok(cfg)
}

fn api_post_json(cfg: &Config, url: &str, body: Value) -> Result<Value, String> {
    let response = ureq::post(url)
        .set("Authorization", &format!("Bearer {}", cfg.access_token))
        .set("Content-Type", "application/json")
        .send_string(&body.to_string())
        .map_err(http_error)?;
    let text = response.into_string().map_err(|err| err.to_string())?;
    serde_json::from_str::<Value>(&text).map_err(|err| err.to_string())
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
