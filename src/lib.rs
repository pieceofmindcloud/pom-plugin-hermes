//! Hermes Agent dashboard integration. POM loads this cdylib in an isolated host.
mod abi;
mod context;
mod installer;
mod supervisor;

use context::Context;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use supervisor::Supervisor;

pub const PLUGIN_CODE: &str = "hermes";
const UI_MANIFEST: &str = include_str!("../ui/manifest.json");
include!(concat!(env!("OUT_DIR"), "/ui_assets.rs"));

/// # Safety
/// Immutable ABI table valid for the lifetime of the library.
#[no_mangle]
pub unsafe extern "C" fn pom_hermes_plugin_v1() -> *const abi::PluginApiV1 {
    &abi::API
}

pub struct Plugin {
    supervisor: Arc<Supervisor>,
}
impl Drop for Plugin {
    fn drop(&mut self) {
        self.supervisor.stop();
    }
}
impl Plugin {
    pub fn new(_config: &Value) -> Result<Self, String> {
        Ok(Self {
            supervisor: Supervisor::new(),
        })
    }
    pub fn ingest(&self, event: &Value) -> Result<(), String> {
        event["type"].as_str().ok_or("missing event type")?;
        Ok(())
    }
    pub fn query(&self, request: &Value) -> Result<Value, String> {
        match request["operation"].as_str().unwrap_or_default() {
            "host.configure" => {
                self.supervisor.configure(Context::from_configure(request)?);
                Ok(json!({"status":"ok"}))
            }
            "host.event" => Ok(json!({"status":"ok"})),
            "host.services" => {
                let status = self.supervisor.status();
                Ok(
                    json!({"services":[{"id":"agent","kind":"agent","name":"Hermes Agent","status": status.phase,"detail":status.detail}]}),
                )
            }
            "ui.upstream" => Ok(self.supervisor.status().upstream()),
            "ui.manifest" => ui_manifest(),
            "ui.asset" => {
                let path = request["path"].as_str().ok_or("missing asset path")?;
                let (_, content_type, bytes) = ui_asset(path).ok_or("unknown asset")?;
                Ok(json!({"content_type":content_type,"base64":encode_base64(bytes)}))
            }
            "rpc.status" => Ok(self.supervisor.status().public()),
            "rpc.install" => {
                // Plugins are local to their node; every node profile may
                // configure the application it installed on its own machine.
                let model = request["params"]["model"]
                    .as_str()
                    .map(str::trim)
                    .filter(|m| !m.is_empty())
                    .map(str::to_owned);
                Ok(json!({"status":if self.supervisor.launch(model) {"starting"} else {"busy"}}))
            }
            _ => Err("unknown operation".into()),
        }
    }
}

/// Fixed files outside the shared workspace; user data lives under `.hermes` there.
pub fn plugin_directory() -> Result<PathBuf, String> {
    let parent = std::env::var_os("POM_PLUGIN_DB")
        .map(PathBuf::from)
        .and_then(|db| db.parent().map(Path::to_path_buf))
        .filter(|p| !p.as_os_str().is_empty());
    Ok(parent
        .unwrap_or(std::env::current_dir().map_err(|e| e.to_string())?)
        .join(PLUGIN_CODE))
}
fn ui_asset(path: &str) -> Option<&'static (&'static str, &'static str, &'static [u8])> {
    UI_ASSETS.iter().find(|(name, _, _)| *name == path)
}
fn ui_manifest() -> Result<Value, String> {
    let manifest: Value = serde_json::from_str(UI_MANIFEST).map_err(|e| e.to_string())?;
    for path in manifest["assets"]
        .as_array()
        .ok_or("manifest has no assets")?
        .iter()
        .filter_map(Value::as_str)
    {
        if ui_asset(path).is_none() {
            return Err(format!("asset not embedded: {path}"));
        }
    }
    Ok(manifest)
}
fn encode_base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let value = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        for index in 0..4 {
            output.push(if index <= chunk.len() {
                TABLE[((value >> (18 - 6 * index)) & 63) as usize] as char
            } else {
                '='
            });
        }
    }
    output
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn manifest_and_abi() {
        assert_eq!(
            unsafe { (*pom_hermes_plugin_v1()).abi_version },
            abi::ABI_VERSION
        );
        if ui_asset("ui/screens.js").is_some() {
            assert!(ui_manifest().is_ok());
        }
    }
    #[test]
    fn no_proxy_before_install() {
        let plugin = Plugin::new(&Value::Null).unwrap();
        assert_eq!(
            plugin.query(&json!({"operation":"ui.upstream"})).unwrap()["status"],
            "starting"
        );
        assert!(!plugin
            .query(&json!({"operation":"rpc.status"}))
            .unwrap()
            .to_string()
            .contains("token"));
    }
}
