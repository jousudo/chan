//! Team template storage backed by `~/.chan/team-templates/`.
//!
//! Templates are globally stored TeamConfig TOML files (not inside any
//! workspace) so a template can be applied to any workspace and shared
//! between projects or across a company standard.
//!
//! Storage layout:
//!
//! ```text
//! ~/.chan/team-templates/
//!   <name>.toml   one file per template; IS a valid TeamConfig TOML
//! ```
//!
//! The file format is the same TeamConfig schema the per-workspace
//! `config.toml` uses, so export/import are byte-identical round-trips
//! and the files are hand-editable. `created_at` is intentionally left
//! blank in templates; the server stamps it on the next bootstrap write.
//!
//! Write paths use `crate::store::save_toml` for atomic rename + parent-dir
//! fsync, matching the rest of the app-level config discipline.

use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{Multipart, Path, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use chan_workspace::TeamConfig;
use serde::Serialize;

use crate::error::err;
use crate::routes::team_config::validate_team_config;
use crate::state::AppState;
use crate::store;

fn templates_dir() -> PathBuf {
    chan_workspace::paths::config_dir().join("team-templates")
}

/// Validate and normalize a template name to a safe filename stem.
/// Accepts a-z, 0-9, hyphens, and underscores; lowercases; trims.
/// Returns the normalized name or a human-readable error for the 400 body.
fn require_safe_name(raw: &str) -> Result<String, String> {
    let s = raw.trim().to_ascii_lowercase();
    if s.is_empty() || s.len() > 100 {
        return Err("template name must be 1-100 characters".into());
    }
    if s.starts_with('-') {
        return Err("template name must not start with a hyphen".into());
    }
    // Guard against path traversal before the `.toml` suffix is appended.
    if s.contains('/') || s.contains('\\') || s.contains("..") {
        return Err("template name may not contain path separators".into());
    }
    if !s
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err("template name may only contain a-z, 0-9, hyphens, and underscores".into());
    }
    Ok(s)
}

/// Derive a safe template name slug from a raw `team_name` string: lowercase,
/// replace disallowed chars with hyphens, collapse runs, trim edge hyphens.
/// Falls back to "team" when no safe chars survive.
fn slug_from_team_name(team_name: &str) -> String {
    let raw: String = team_name
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    // Collapse consecutive hyphens introduced by multi-char separators.
    let mut s = raw;
    while s.contains("--") {
        s = s.replace("--", "-");
    }
    let s = s.trim_matches('-');
    if s.is_empty() {
        "team".to_string()
    } else {
        s[..s.len().min(100)].to_string()
    }
}

/// Lightweight summary returned by `GET /api/team-templates`.
#[derive(Debug, Serialize)]
pub struct TemplateInfo {
    pub name: String,
    pub team_name: String,
    pub member_count: usize,
}

/// `GET /api/team-templates` - list all saved templates, sorted by name.
pub async fn api_list_team_templates(State(_state): State<Arc<AppState>>) -> Response {
    let result = tokio::task::spawn_blocking(list_templates).await;
    match result {
        Ok(Ok(list)) => Json(list).into_response(),
        Ok(Err(msg)) => err(StatusCode::INTERNAL_SERVER_ERROR, msg),
        Err(join) => err(StatusCode::INTERNAL_SERVER_ERROR, join.to_string()),
    }
}

fn list_templates() -> Result<Vec<TemplateInfo>, String> {
    let dir = templates_dir();
    if !dir.exists() {
        return Ok(vec![]);
    }
    let rd = std::fs::read_dir(&dir).map_err(|e| format!("cannot list templates: {e}"))?;
    let mut out: Vec<TemplateInfo> = Vec::new();
    for entry in rd.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        // Skip files whose stem is not a valid name (e.g. partial writes left
        // by a crash before atomic rename; `require_safe_name` rejects them).
        if require_safe_name(stem).is_err() {
            continue;
        }
        let name = stem.to_string();
        let Ok(raw) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(cfg) = toml::from_str::<TeamConfig>(&raw) else {
            continue;
        };
        out.push(TemplateInfo {
            name,
            team_name: cfg.team_name.clone(),
            member_count: cfg.members.len(),
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// `POST /api/team-templates` body.
#[derive(serde::Deserialize)]
pub struct SaveTemplatePayload {
    pub name: String,
    pub config: TeamConfig,
}

/// `POST /api/team-templates` - validate and save a named template.
pub async fn api_save_team_template(
    State(_state): State<Arc<AppState>>,
    Json(payload): Json<SaveTemplatePayload>,
) -> Response {
    let name = match require_safe_name(&payload.name) {
        Ok(n) => n,
        Err(msg) => return err(StatusCode::BAD_REQUEST, msg),
    };
    let config = payload.config;
    let result = tokio::task::spawn_blocking(move || save_template(&name, &config)).await;
    match result {
        Ok(Ok(())) => Json(serde_json::json!({})).into_response(),
        Ok(Err(msg)) => err(StatusCode::BAD_REQUEST, msg),
        Err(join) => err(StatusCode::INTERNAL_SERVER_ERROR, join.to_string()),
    }
}

fn save_template(name: &str, config: &TeamConfig) -> Result<(), String> {
    validate_team_config(config)?;
    let path = templates_dir().join(format!("{name}.toml"));
    store::save_toml(&path, config).map_err(|e| format!("cannot save template: {e}"))
}

/// `GET /api/team-templates/:name` - read one template's full config.
/// Used by the "Load from template" flow to pre-populate the dialog.
pub async fn api_get_team_template(
    State(_state): State<Arc<AppState>>,
    Path(raw_name): Path<String>,
) -> Response {
    let name = match require_safe_name(&raw_name) {
        Ok(n) => n,
        Err(msg) => return err(StatusCode::BAD_REQUEST, msg),
    };
    let result = tokio::task::spawn_blocking(move || load_template(&name)).await;
    match result {
        Ok(Ok(config)) => Json(config).into_response(),
        Ok(Err(msg)) => err(StatusCode::NOT_FOUND, msg),
        Err(join) => err(StatusCode::INTERNAL_SERVER_ERROR, join.to_string()),
    }
}

fn load_template(name: &str) -> Result<TeamConfig, String> {
    let path = templates_dir().join(format!("{name}.toml"));
    let raw = std::fs::read_to_string(&path).map_err(|_| format!("template '{name}' not found"))?;
    toml::from_str::<TeamConfig>(&raw).map_err(|e| format!("invalid template '{name}': {e}"))
}

/// `DELETE /api/team-templates/:name` - delete a saved template.
pub async fn api_delete_team_template(
    State(_state): State<Arc<AppState>>,
    Path(raw_name): Path<String>,
) -> Response {
    let name = match require_safe_name(&raw_name) {
        Ok(n) => n,
        Err(msg) => return err(StatusCode::BAD_REQUEST, msg),
    };
    let result = tokio::task::spawn_blocking(move || delete_template(&name)).await;
    match result {
        Ok(Ok(())) => Json(serde_json::json!({})).into_response(),
        Ok(Err(msg)) => err(StatusCode::NOT_FOUND, msg),
        Err(join) => err(StatusCode::INTERNAL_SERVER_ERROR, join.to_string()),
    }
}

fn delete_template(name: &str) -> Result<(), String> {
    let path = templates_dir().join(format!("{name}.toml"));
    std::fs::remove_file(&path).map_err(|_| format!("template '{name}' not found"))?;
    Ok(())
}

/// `GET /api/team-templates/:name/export` - download the raw TOML as a
/// file attachment. The wire format IS the on-disk format, so the download
/// can be hand-edited and re-imported unchanged.
pub async fn api_export_team_template(
    State(_state): State<Arc<AppState>>,
    Path(raw_name): Path<String>,
) -> Response {
    let name = match require_safe_name(&raw_name) {
        Ok(n) => n,
        Err(msg) => return err(StatusCode::BAD_REQUEST, msg),
    };
    let filename = format!("{name}.toml");
    let result = tokio::task::spawn_blocking(move || {
        let path = templates_dir().join(format!("{name}.toml"));
        std::fs::read(&path).map_err(|_| format!("template '{name}' not found"))
    })
    .await;
    match result {
        Ok(Ok(bytes)) => {
            let mut resp = bytes.into_response();
            let headers = resp.headers_mut();
            headers.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/toml"),
            );
            if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"{filename}\"")) {
                headers.insert(header::CONTENT_DISPOSITION, v);
            }
            resp
        }
        Ok(Err(msg)) => err(StatusCode::NOT_FOUND, msg),
        Err(join) => err(StatusCode::INTERNAL_SERVER_ERROR, join.to_string()),
    }
}

/// `POST /api/team-templates/import` - import a template from an uploaded
/// TOML file. Multipart fields:
///   - `file` (required): TeamConfig TOML bytes
///   - `name` (optional): override the saved name; derived from `team_name`
///     when absent (slugified)
///
/// Returns `{"name": "<saved-name>"}` so the SPA can refresh its list and
/// show the newly imported template.
pub async fn api_import_team_template(
    State(_state): State<Arc<AppState>>,
    mut multipart: Multipart,
) -> Response {
    let mut file_bytes: Option<Vec<u8>> = None;
    let mut name_override: Option<String> = None;

    loop {
        match multipart.next_field().await {
            Ok(Some(field)) => {
                let field_name = field.name().unwrap_or("").to_owned();
                match field_name.as_str() {
                    "file" if file_bytes.is_none() => match field.bytes().await {
                        Ok(b) => file_bytes = Some(b.to_vec()),
                        Err(e) => {
                            return err(StatusCode::BAD_REQUEST, format!("multipart read: {e}"));
                        }
                    },
                    "name" => match field.text().await {
                        Ok(n) if !n.trim().is_empty() => name_override = Some(n),
                        Ok(_) => {}
                        Err(e) => {
                            return err(StatusCode::BAD_REQUEST, format!("multipart read: {e}"));
                        }
                    },
                    _ => {
                        let _ = field.bytes().await;
                    }
                }
            }
            Ok(None) => break,
            Err(e) => {
                return err(StatusCode::BAD_REQUEST, format!("multipart parse: {e}"));
            }
        }
    }

    let Some(bytes) = file_bytes else {
        return err(StatusCode::BAD_REQUEST, "missing `file` part".into());
    };
    if bytes.is_empty() {
        return err(StatusCode::BAD_REQUEST, "empty template file".into());
    }

    let result = tokio::task::spawn_blocking(move || import_template(bytes, name_override)).await;
    match result {
        Ok(Ok(name)) => Json(serde_json::json!({ "name": name })).into_response(),
        Ok(Err(msg)) => err(StatusCode::BAD_REQUEST, msg),
        Err(join) => err(StatusCode::INTERNAL_SERVER_ERROR, join.to_string()),
    }
}

fn import_template(bytes: Vec<u8>, name_override: Option<String>) -> Result<String, String> {
    let text =
        std::str::from_utf8(&bytes).map_err(|_| "template file is not valid UTF-8".to_string())?;
    let config: TeamConfig =
        toml::from_str(text).map_err(|e| format!("invalid team config TOML: {e}"))?;
    validate_team_config(&config)?;
    let name = match name_override {
        Some(n) => require_safe_name(&n)?,
        None => slug_from_team_name(&config.team_name),
    };
    save_template(&name, &config)?;
    Ok(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn require_safe_name_normalizes_to_lowercase() {
        assert_eq!(require_safe_name("Alpha-1").unwrap(), "alpha-1");
        assert_eq!(require_safe_name("  MyTeam  ").unwrap(), "myteam");
    }

    #[test]
    fn require_safe_name_rejects_bad_inputs() {
        assert!(require_safe_name("").is_err(), "empty");
        assert!(require_safe_name("-bad").is_err(), "leading hyphen");
        assert!(require_safe_name("../escape").is_err(), "path traversal");
        assert!(require_safe_name("no spaces").is_err(), "space");
        assert!(require_safe_name("a/b").is_err(), "slash");
    }

    #[test]
    fn require_safe_name_rejects_overlong_name() {
        let long = "a".repeat(101);
        assert!(require_safe_name(&long).is_err());
    }

    #[test]
    fn slug_from_team_name_converts_spaces_to_hyphens() {
        assert_eq!(slug_from_team_name("My Team Alpha"), "my-team-alpha");
    }

    #[test]
    fn slug_from_team_name_collapses_runs() {
        assert_eq!(slug_from_team_name("A!!B"), "a-b");
    }

    #[test]
    fn slug_from_team_name_falls_back_to_team() {
        assert_eq!(slug_from_team_name("  "), "team");
        assert_eq!(slug_from_team_name("!!!"), "team");
    }

    #[test]
    fn slug_from_team_name_respects_max_length() {
        let long = "a".repeat(200);
        let slug = slug_from_team_name(&long);
        assert!(slug.len() <= 100);
    }
}
