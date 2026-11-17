pub mod db;
pub mod engine;
pub mod error;
pub mod llm;
pub mod mcp;
pub mod skills;
pub mod speech;
pub mod state;
pub mod telegram;
pub mod tools;

use engine::task_contract::TaskContract;
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;
use tauri::ipc::Channel;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{Manager, State};
use tokio::sync::RwLock;

pub struct AppManagedState {
    pub app_state: Arc<state::AppState>,
    pub stream_mgr: Arc<engine::StreamManager>,
    pub mcp_catalog: Arc<mcp::McpServerCatalog>,
    pub telegram_mgr: Arc<telegram::TelegramBotManager>,
    pub speech_engine: Arc<speech::SpeechEngine>,
    pub tool_dispatcher: Arc<tools::ToolDispatcher>,
    pub key_pool: Arc<llm::KeyPool>,
    pub caps_registry: Arc<llm::CapabilityRegistry>,
    pub http_client: reqwest::Client,
    pub active_contracts: Arc<RwLock<std::collections::HashMap<String, Arc<tokio::sync::Mutex<Option<TaskContract>>>>>>,
}

impl AppManagedState {
    pub fn has_active_tasks(&self) -> bool {
        let has_streams = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(self.stream_mgr.has_running_sessions())
        });
        let has_bot = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(self.telegram_mgr.is_running())
        });
        has_streams || has_bot
    }
}

fn build_http_client(
    proxy_enabled: Option<bool>,
    proxy_url: Option<&str>,
    fallback: Option<&reqwest::Client>,
) -> Result<reqwest::Client, String> {
    if proxy_enabled.unwrap_or(false) {
        if let Some(purl) = proxy_url.map(|s| s.trim()).filter(|s| !s.is_empty()) {
            let proxy = reqwest::Proxy::all(purl).map_err(|e| format!("Invalid proxy URL: {}", e))?;
            return reqwest::Client::builder()
                .proxy(proxy)
                .connect_timeout(std::time::Duration::from_secs(20))
                .build()
                .map_err(|e| e.to_string());
        }
    }
    if let Some(c) = fallback {
        return Ok(c.clone());
    }
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn window_show(app: tauri::AppHandle, window: tauri::Window) {
    if let Some(icon) = app.default_window_icon() {
        let _ = window.set_icon(icon.clone());
    }
    let _ = window.show();
    let _ = window.set_focus();
}

#[tauri::command]
fn window_minimize(window: tauri::Window) {
    let _ = window.minimize();
}

#[tauri::command]
fn window_toggle_fullscreen(window: tauri::Window) -> Result<bool, String> {
    let is_fs = window.is_fullscreen().map_err(|e| e.to_string())?;
    window.set_fullscreen(!is_fs).map_err(|e| e.to_string())?;
    Ok(!is_fs)
}

#[tauri::command]
fn window_toggle_maximize(window: tauri::Window) {
    if window.is_maximized().unwrap_or(false) {
        let _ = window.unmaximize();
    } else {
        let _ = window.maximize();
    }
}

#[tauri::command]
fn window_close(window: tauri::Window) {
    let _ = window.close();
}

#[tauri::command]
fn window_start_dragging(window: tauri::Window) {
    let _ = window.start_dragging();
}

#[tauri::command]
async fn create_project(
    state: State<'_, AppManagedState>,
    project: db::chats::Project,
) -> Result<db::chats::Project, String> {
    state
        .app_state
        .db
        .create_project(&project)
        .map_err(|e| e.to_string())?;
    Ok(project)
}

#[tauri::command]
async fn list_projects(
    state: State<'_, AppManagedState>,
) -> Result<Vec<db::chats::Project>, String> {
    state.app_state.db.list_projects().map_err(|e| e.to_string())
}

#[tauri::command]
async fn get_project(
    state: State<'_, AppManagedState>,
    id: String,
) -> Result<db::chats::Project, String> {
    state
        .app_state
        .db
        .get_project(&id)
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn delete_project(state: State<'_, AppManagedState>, id: String) -> Result<(), String> {
    state
        .app_state
        .db
        .delete_project(&id)
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn create_chat(
    state: State<'_, AppManagedState>,
    chat: db::chats::Chat,
) -> Result<db::chats::Chat, String> {
    state
        .app_state
        .db
        .create_chat(&chat)
        .map_err(|e| e.to_string())?;
    Ok(chat)
}

#[tauri::command]
async fn list_chats(
    state: State<'_, AppManagedState>,
    project_id: Option<String>,
) -> Result<Vec<db::chats::Chat>, String> {
    state
        .app_state
        .db
        .list_chats(project_id.as_deref())
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn get_chat(state: State<'_, AppManagedState>, id: String) -> Result<db::chats::Chat, String> {
    state.app_state.db.get_chat(&id).map_err(|e| e.to_string())
}

#[tauri::command]
async fn update_chat_title(
    state: State<'_, AppManagedState>,
    id: String,
    title: String,
) -> Result<(), String> {
    state
        .app_state
        .db
        .update_chat_title(&id, &title)
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn update_chat_pinned(
    state: State<'_, AppManagedState>,
    id: String,
    is_pinned: bool,
) -> Result<(), String> {
    state
        .app_state
        .db
        .update_chat_pinned(&id, is_pinned)
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn delete_chat(state: State<'_, AppManagedState>, id: String) -> Result<(), String> {
    let _ = state.stream_mgr.cancel_session(&id).await;
    let _ = state.tool_dispatcher.pty_manager.kill_all_for_chat(&id);
    let chat_ws = state::AppState::resolve_install_workspace().join(&id);
    if chat_ws.exists() {
        let _ = std::fs::remove_dir_all(&chat_ws);
    }
    let chat_tmp = state::AppState::resolve_chat_temp_dir(Some(&id));
    if chat_tmp.exists() {
        let _ = std::fs::remove_dir_all(&chat_tmp);
    }
    state
        .app_state
        .db
        .delete_chat(&id)
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn read_file_full(path: String) -> Result<String, String> {
    tokio::task::spawn_blocking(move || std::fs::read_to_string(&path).map_err(|e| e.to_string()))
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn create_message(
    state: State<'_, AppManagedState>,
    message: db::chats::Message,
) -> Result<db::chats::Message, String> {
    state
        .app_state
        .db
        .create_message(&message)
        .map_err(|e| e.to_string())?;
    Ok(message)
}

#[tauri::command]
async fn list_messages(
    state: State<'_, AppManagedState>,
    chat_id: String,
) -> Result<Vec<db::chats::Message>, String> {
    state
        .app_state
        .db
        .list_messages(&chat_id)
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn delete_messages_since(
    state: State<'_, AppManagedState>,
    chat_id: String,
    sequence_num: i64,
) -> Result<(), String> {
    state
        .app_state
        .db
        .delete_messages_since(&chat_id, sequence_num)
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn delete_message(state: State<'_, AppManagedState>, id: String) -> Result<(), String> {
    state
        .app_state
        .db
        .delete_message(&id)
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn record_file_snapshot(
    state: State<'_, AppManagedState>,
    snapshot: db::FileSnapshot,
) -> Result<(), String> {
    state
        .app_state
        .db
        .record_file_snapshot(&snapshot)
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn get_file_snapshot(
    state: State<'_, AppManagedState>,
    snapshot_id: String,
) -> Result<db::FileSnapshot, String> {
    state
        .app_state
        .db
        .get_file_snapshot(&snapshot_id)
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn list_snapshots_for_chat(
    state: State<'_, AppManagedState>,
    chat_id: String,
) -> Result<Vec<db::FileSnapshot>, String> {
    state
        .app_state
        .db
        .list_snapshots_for_chat(&chat_id)
        .map_err(|e| e.to_string())
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct TurnFileDiff {
    path: String,
    before_text: String,
    after_text: String,
    added_lines: i64,
    removed_lines: i64,
    existed_before: bool,
}

#[tauri::command]
async fn get_turn_file_diffs(
    state: State<'_, AppManagedState>,
    snapshot_ids: Vec<String>,
) -> Result<Vec<TurnFileDiff>, String> {
    let mut order: Vec<String> = Vec::new();
    let mut by_path: std::collections::HashMap<String, (Option<Vec<u8>>, Option<Vec<u8>>, i64, i64, bool)> =
        std::collections::HashMap::new();

    for sid in snapshot_ids {
        if let Ok(snap) = state.app_state.db.get_file_snapshot(&sid) {
            let entry = by_path.entry(snap.file_path.clone()).or_insert_with(|| {
                order.push(snap.file_path.clone());
                (snap.before_blob.clone(), snap.after_blob.clone(), 0, 0, snap.existed_before)
            });
            entry.1 = snap.after_blob;
            entry.2 += snap.added_lines;
            entry.3 += snap.removed_lines;
        }
    }

    let mut out = Vec::with_capacity(order.len());
    for path in order {
        if let Some((before_b, after_b, added, removed, existed_before)) = by_path.remove(&path) {
            let before_text = before_b
                .map(|b| String::from_utf8_lossy(&b).to_string())
                .unwrap_or_default();
            let after_text = after_b
                .map(|b| String::from_utf8_lossy(&b).to_string())
                .unwrap_or_default();
            out.push(TurnFileDiff {
                path,
                before_text,
                after_text,
                added_lines: added,
                removed_lines: removed,
                existed_before,
            });
        }
    }
    Ok(out)
}

#[tauri::command]
async fn rollback_file_snapshot(
    state: State<'_, AppManagedState>,
    snapshot_id: String,
) -> Result<db::FileSnapshot, String> {
    state
        .app_state
        .db
        .rollback_file_snapshot(&snapshot_id)
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn get_events_since(
    state: State<'_, AppManagedState>,
    session_id: String,
    last_seq: i64,
) -> Result<Vec<db::EventRecord>, String> {
    state
        .app_state
        .db
        .list_events_since(&session_id, last_seq)
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn list_session_checkpoints(
    state: State<'_, AppManagedState>,
    chat_id: String,
) -> Result<Vec<db::CheckpointRecord>, String> {
    state
        .app_state
        .db
        .list_checkpoints(&chat_id)
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn rewind_checkpoint(
    state: State<'_, AppManagedState>,
    chat_id: String,
    checkpoint_id: String,
    mode: String,
) -> Result<tools::blob_store::RewindResult, String> {
    let cp = state
        .app_state
        .db
        .get_checkpoint(&checkpoint_id)
        .map_err(|e| e.to_string())?;

    let norm_mode = mode.trim().to_lowercase();
    let do_code = norm_mode == "code" || norm_mode == "both";
    let do_conv = norm_mode == "conversation" || norm_mode == "both";
    if !do_code && !do_conv {
        return Err("Invalid rewind mode. Use code, conversation, or both.".to_string());
    }

    let mut result = tools::blob_store::RewindResult {
        checkpoint_id: checkpoint_id.clone(),
        mode: norm_mode.clone(),
        restored_files: Vec::new(),
        deleted_files: Vec::new(),
        skipped_large_files: Vec::new(),
        dirty_external_files: Vec::new(),
    };

    if do_code {
        let manifest: tools::blob_store::CheckpointManifest =
            serde_json::from_str(&cp.manifest_json).map_err(|e| e.to_string())?;
        let mut expected_hashes: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        if let Ok(evts) = state.app_state.db.list_events_since(&chat_id, 0) {
            for ev in evts {
                if ev.kind == "file_patched" {
                    if let (Some(fp), Some(ah)) = (
                        ev.payload.get("path").and_then(|v| v.as_str()),
                        ev.payload.get("afterHash").and_then(|v| v.as_str()),
                    ) {
                        if !fp.is_empty() && !ah.is_empty() {
                            let rel = std::path::Path::new(fp)
                                .strip_prefix(&manifest.workspace_root)
                                .map(|r| r.to_string_lossy().replace('\\', "/"))
                                .unwrap_or_else(|_| fp.replace('\\', "/"));
                            expected_hashes.insert(rel, ah.to_string());
                        }
                    }
                }
            }
        }
        let code_res = tools::blob_store::atomic_restore_batch(&manifest, Some(&expected_hashes))?;
        result.restored_files = code_res.restored_files;
        result.deleted_files = code_res.deleted_files;
        result.skipped_large_files = code_res.skipped_large_files;
        result.dirty_external_files = code_res.dirty_external_files;
    }

    if do_conv {
        state
            .app_state
            .db
            .truncate_messages_after_seq(&chat_id, cp.message_seq)
            .map_err(|e| e.to_string())?;
    }

    state.app_state.db.emit_event(
        &chat_id,
        "rewind",
        serde_json::json!({
            "checkpointId": checkpoint_id,
            "mode": norm_mode,
            "restoredFiles": result.restored_files,
            "skippedLargeFiles": result.skipped_large_files,
            "dirtyExternalFiles": result.dirty_external_files
        }),
    );

    Ok(result)
}

#[tauri::command]
async fn record_usage_stat(
    state: State<'_, AppManagedState>,
    stat: db::chats::ModelUsageStat,
) -> Result<(), String> {
    state
        .app_state
        .db
        .record_usage_stat(&stat)
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn list_usage_stats(
    state: State<'_, AppManagedState>,
    provider_id: Option<String>,
    model_id: Option<String>,
) -> Result<Vec<db::chats::ModelUsageStat>, String> {
    state
        .app_state
        .db
        .list_usage_stats(provider_id.as_deref(), model_id.as_deref())
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn create_provider(
    state: State<'_, AppManagedState>,
    provider: db::settings::Provider,
) -> Result<(), String> {
    state
        .app_state
        .db
        .create_provider(&provider)
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn list_providers(
    state: State<'_, AppManagedState>,
) -> Result<Vec<db::settings::Provider>, String> {
    state
        .app_state
        .db
        .list_providers()
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn get_provider(
    state: State<'_, AppManagedState>,
    id: String,
) -> Result<db::settings::Provider, String> {
    state
        .app_state
        .db
        .get_provider(&id)
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn update_provider(
    state: State<'_, AppManagedState>,
    provider: db::settings::Provider,
) -> Result<(), String> {
    state
        .app_state
        .db
        .update_provider(&provider)
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn delete_provider(state: State<'_, AppManagedState>, id: String) -> Result<(), String> {
    state
        .app_state
        .db
        .delete_provider(&id)
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn ping_provider(state: State<'_, AppManagedState>, id: String) -> Result<bool, String> {
    let provider = state
        .app_state
        .db
        .get_provider(&id)
        .map_err(|e| e.to_string())?;

    let client = reqwest::Client::new();
    let res = client
        .get(&provider.base_url)
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await;

    Ok(res.is_ok())
}

#[tauri::command]
async fn create_model(
    state: State<'_, AppManagedState>,
    model: db::settings::Model,
) -> Result<(), String> {
    state
        .app_state
        .db
        .create_model(&model)
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn list_models(
    state: State<'_, AppManagedState>,
    provider_id: Option<String>,
) -> Result<Vec<db::settings::Model>, String> {
    state
        .app_state
        .db
        .list_models(provider_id.as_deref())
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn get_model(
    state: State<'_, AppManagedState>,
    id: String,
) -> Result<db::settings::Model, String> {
    state
        .app_state
        .db
        .get_model(&id)
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn update_model(
    state: State<'_, AppManagedState>,
    model: db::settings::Model,
) -> Result<(), String> {
    state
        .app_state
        .db
        .update_model(&model)
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn delete_model(state: State<'_, AppManagedState>, id: String) -> Result<(), String> {
    state
        .app_state
        .db
        .delete_model(&id)
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn create_mcp_server(
    state: State<'_, AppManagedState>,
    server: db::settings::McpServer,
) -> Result<(), String> {
    state
        .app_state
        .db
        .create_mcp_server(&server)
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn list_mcp_servers(
    state: State<'_, AppManagedState>,
) -> Result<Vec<db::settings::McpServer>, String> {
    state
        .app_state
        .db
        .list_mcp_servers()
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn get_mcp_server(
    state: State<'_, AppManagedState>,
    id: String,
) -> Result<db::settings::McpServer, String> {
    state
        .app_state
        .db
        .get_mcp_server(&id)
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn update_mcp_server(
    state: State<'_, AppManagedState>,
    server: db::settings::McpServer,
) -> Result<(), String> {
    state
        .app_state
        .db
        .update_mcp_server(&server)
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn delete_mcp_server(state: State<'_, AppManagedState>, id: String) -> Result<(), String> {
    state
        .app_state
        .db
        .delete_mcp_server(&id)
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn connect_mcp_server(
    state: State<'_, AppManagedState>,
    server: mcp::McpServerConfig,
) -> Result<(), String> {
    state.mcp_catalog.connect_server(&server).await
}

#[tauri::command]
async fn list_mcp_tools(
    state: State<'_, AppManagedState>,
) -> Result<Vec<mcp::McpToolWithServer>, String> {
    Ok(state.mcp_catalog.list_all_tools().await)
}

#[tauri::command]
async fn disconnect_mcp_server(
    state: State<'_, AppManagedState>,
    name: String,
) -> Result<(), String> {
    state.mcp_catalog.disconnect_server(&name).await
}

#[tauri::command]
async fn import_mcp_config(
    state: State<'_, AppManagedState>,
    json_config: String,
) -> Result<usize, String> {
    let configs = mcp::McpServerCatalog::import_json_config(&json_config)?;
    let count = configs.len();
    for cfg in configs {
        let _ = state.mcp_catalog.connect_server(&cfg).await;
    }
    Ok(count)
}

#[tauri::command]
async fn call_mcp_tool(
    state: State<'_, AppManagedState>,
    server_name: String,
    tool_name: String,
    arguments: Option<serde_json::Value>,
) -> Result<mcp::McpCallToolResult, String> {
    state
        .mcp_catalog
        .call_tool(&server_name, &tool_name, arguments)
        .await
}

#[tauri::command]
async fn create_memory_entry(
    state: State<'_, AppManagedState>,
    entry: db::memory::MemoryEntry,
) -> Result<(), String> {
    state
        .app_state
        .db
        .create_memory_entry(&entry)
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn list_memory_entries(
    state: State<'_, AppManagedState>,
    category: Option<String>,
) -> Result<Vec<db::memory::MemoryEntry>, String> {
    state
        .app_state
        .db
        .list_memory_entries(category.as_deref())
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn get_memory_entry(
    state: State<'_, AppManagedState>,
    id: String,
) -> Result<db::memory::MemoryEntry, String> {
    state
        .app_state
        .db
        .get_memory_entry(&id)
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn update_memory_entry(
    state: State<'_, AppManagedState>,
    entry: db::memory::MemoryEntry,
) -> Result<(), String> {
    state
        .app_state
        .db
        .update_memory_entry(&entry)
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn delete_memory_entry(state: State<'_, AppManagedState>, id: String) -> Result<(), String> {
    state
        .app_state
        .db
        .delete_memory_entry(&id)
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn search_memory_entries(
    state: State<'_, AppManagedState>,
    query: String,
) -> Result<Vec<db::memory::MemoryEntry>, String> {
    state
        .app_state
        .db
        .search_memory_entries(&query)
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn get_total_memory_tokens(state: State<'_, AppManagedState>) -> Result<i64, String> {
    state
        .app_state
        .db
        .get_total_memory_tokens()
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn sync_chat_context(
    state: State<'_, AppManagedState>,
    chat_id: String,
    title: String,
    messages_json: String,
    project_path: Option<String>,
    model_id: Option<String>,
) -> Result<(), String> {
    let db = &state.app_state.db;
    db.sync_chat_context(&chat_id, &title, &messages_json)
        .map_err(|e| e.to_string())?;

    let now = chrono::Utc::now().to_rfc3339();
    if db.get_chat(&chat_id).is_err() {
        let chat = db::chats::Chat {
            id: chat_id.clone(),
            project_id: project_path.clone().filter(|s| !s.trim().is_empty()),
            title: title.clone(),
            model_id: model_id.clone().unwrap_or_default(),
            system_prompt: String::new(),
            is_pinned: false,
            created_at: now.clone(),
            updated_at: now.clone(),
        };
        let _ = db.create_chat(&chat);
    } else if !title.trim().is_empty() {
        let _ = db.update_chat_title(&chat_id, &title);
    }

    if let Ok(parsed) = serde_json::from_str::<Vec<serde_json::Value>>(&messages_json) {
        let existing = db.list_messages(&chat_id).unwrap_or_default();
        if parsed.len() > existing.len() {
            let mut seq = existing.last().map(|m| m.sequence_num).unwrap_or(0);
            for item in parsed.iter().skip(existing.len()) {
                let role = item.get("role").and_then(|v| v.as_str()).unwrap_or("user").to_string();
                let content = item.get("content").and_then(|v| v.as_str()).unwrap_or("").to_string();
                if content.trim().is_empty() {
                    continue;
                }
                seq += 1;
                let msg = db::chats::Message {
                    id: uuid::Uuid::new_v4().to_string(),
                    chat_id: chat_id.clone(),
                    role,
                    content,
                    reasoning_content: None,
                    tool_calls: None,
                    tool_call_id: None,
                    sequence_num: seq,
                    created_at: now.clone(),
                };
                let _ = db.create_message(&msg);
            }
        }
    }
    Ok(())
}

#[tauri::command]
async fn sync_workspaces_to_backend(
    state: State<'_, AppManagedState>,
    workspaces: Vec<telegram::SyncedProject>,
) -> Result<(), String> {
    state.telegram_mgr.sync_projects(workspaces).await
}

#[tauri::command]
async fn get_bootstrap_config(
    state: State<'_, AppManagedState>,
) -> Result<state::BootstrapConfig, String> {
    Ok(state.app_state.get_bootstrap_config().await)
}

#[tauri::command]
async fn complete_first_run(
    state: State<'_, AppManagedState>,
    workspace_dir: Option<String>,
    default_model_id: Option<String>,
) -> Result<(), String> {
    state
        .app_state
        .complete_first_run(workspace_dir, default_model_id)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn get_workspace_dir(state: State<'_, AppManagedState>) -> Result<String, String> {
    let ws = state.app_state.get_workspace_dir().await;
    Ok(ws.to_string_lossy().to_string())
}

#[tauri::command]
async fn set_workspace_dir(state: State<'_, AppManagedState>, path: String) -> Result<(), String> {
    state
        .app_state
        .set_workspace_dir(PathBuf::from(path))
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn get_credential(
    state: State<'_, AppManagedState>,
    key_name: String,
) -> Result<String, String> {
    state
        .app_state
        .get_credential(&key_name)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn set_credential(
    state: State<'_, AppManagedState>,
    key_name: String,
    secret: String,
) -> Result<(), String> {
    state
        .app_state
        .set_credential(&key_name, &secret)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn delete_credential(
    state: State<'_, AppManagedState>,
    key_name: String,
) -> Result<(), String> {
    state
        .app_state
        .delete_credential(&key_name)
        .await
        .map_err(|e| e.to_string())
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct HistoryInputItem {
    pub role: String,
    pub content: String,
}

#[tauri::command]
async fn start_chat_stream(
    state: State<'_, AppManagedState>,
    chat_id: String,
    prompt: String,
    model_id: String,
    reasoning_effort: String,
    provider_name: Option<String>,
    base_url: Option<String>,
    api_key: Option<String>,
    api_protocol: Option<String>,
    proxy_enabled: Option<bool>,
    proxy_url: Option<String>,
    context_window: Option<u32>,
    max_output_tokens: Option<u32>,
    supports_reasoning: Option<bool>,
    workspace_dir: Option<String>,
    plan_mode: Option<bool>,
    resume: Option<bool>,
    history: Option<Vec<HistoryInputItem>>,
    on_event: Channel<serde_json::Value>,
) -> Result<String, String> {
    let session = state.stream_mgr.create_session(&chat_id).await;
    let session_id = chat_id.clone();

    let workspace_path = match workspace_dir.as_deref() {
        Some(dir) if !dir.trim().is_empty() => PathBuf::from(dir),
        _ => {
            let app_ws = state.app_state.get_workspace_dir().await;
            if !app_ws.as_os_str().is_empty() && app_ws.exists() && app_ws != state::AppState::resolve_install_workspace() {
                app_ws
            } else {
                let chat_ws = state::AppState::resolve_install_workspace().join(&chat_id);
                let _ = std::fs::create_dir_all(&chat_ws);
                chat_ws
            }
        }
    };
    let workspace_str = workspace_path.to_string_lossy().to_string();

    let tools = state.tool_dispatcher.for_workspace(&workspace_path, &chat_id);
    let combined_plan = plan_mode.unwrap_or(false) || prompt.split_whitespace().any(|t| t.eq_ignore_ascii_case("/plan"));
    tools.set_plan_mode(combined_plan);
    let key_pool = (*state.key_pool).clone();
    let caps_reg = (*state.caps_registry).clone();
    if context_window.is_some() || max_output_tokens.is_some() || supports_reasoning.is_some() {
        caps_reg.update_model_caps(&model_id, context_window, max_output_tokens, supports_reasoning);
    }
    let http_client = build_http_client(proxy_enabled, proxy_url.as_deref(), Some(&state.http_client))?;

    let (mut resolved_provider, mut resolved_url) = ("OpenAI".to_string(), "https://api.openai.com/v1".to_string());
    if let Ok(model_rec) = state.app_state.db.get_model(&model_id) {
        if let Ok(prov_rec) = state.app_state.db.get_provider(&model_rec.provider_id) {
            resolved_provider = prov_rec.name;
            resolved_url = prov_rec.base_url;
        }
    }
    if let Some(pn) = provider_name {
        if !pn.trim().is_empty() {
            resolved_provider = pn;
        }
    }
    if let Some(bu) = base_url {
        if !bu.trim().is_empty() {
            resolved_url = bu;
        }
    }
    if let Some(ak) = api_key {
        if !ak.trim().is_empty() {
            key_pool.set_keys(&resolved_provider, vec![ak.trim().to_string()]);
        }
    }
    let provider_name = resolved_provider;
    let base_url = resolved_url;

    let is_swarm = prompt.starts_with("/swarm");
    let (tx, mut rx) = tokio::sync::mpsc::channel(128);

    let on_event_clone = on_event.clone();
    tokio::spawn(async move {
        while let Some(evt) = rx.recv().await {
            let val = match evt {
                llm::StreamEvent::ContentDelta { delta } => {
                    json!({ "type": "delta", "content": delta })
                }
                llm::StreamEvent::ReasoningDelta { delta } => {
                    json!({ "type": "reasoning_delta", "content": delta })
                }
                llm::StreamEvent::ToolCallDelta {
                    id,
                    name,
                    arguments_delta,
                    ..
                } => {
                    json!({
                        "type": "tool_call_delta",
                        "id": id.unwrap_or_default(),
                        "name": name.unwrap_or_default(),
                        "argumentsDelta": arguments_delta
                    })
                }
                llm::StreamEvent::ToolCallComplete {
                    id,
                    name,
                    arguments,
                    ..
                } => {
                    json!({
                        "type": "tool_call_complete",
                        "id": id,
                        "name": name,
                        "arguments": arguments
                    })
                }
                llm::StreamEvent::ToolOutputDelta { id, chunk } => {
                    json!({
                        "type": "tool_output_delta",
                        "id": id,
                        "chunk": chunk
                    })
                }
                llm::StreamEvent::ToolExecEnd {
                    id,
                    success,
                    output,
                    error,
                    elapsed_ms,
                } => {
                    json!({
                        "type": "tool_end",
                        "id": id,
                        "success": success,
                        "output": output,
                        "error": error,
                        "elapsedMs": elapsed_ms
                    })
                }
                llm::StreamEvent::RetroactiveReasoningMove { text } => {
                    json!({ "type": "retroactive_reasoning_move", "reasoningContent": text })
                }
                llm::StreamEvent::RetryNotice { attempt, reason, .. } => {
                    json!({ "type": "retry_notice", "attempt": attempt, "reason": reason })
                }
                llm::StreamEvent::Finish { usage, .. } => {
                    json!({ "type": "done", "usage": usage })
                }
                llm::StreamEvent::ContextTelemetry {
                    used_tokens,
                    context_window,
                    cached_tokens,
                    cache_hit_rate,
                } => {
                    json!({
                        "type": "context_telemetry",
                        "usedTokens": used_tokens,
                        "contextWindow": context_window,
                        "cachedTokens": cached_tokens,
                        "cacheHitRate": cache_hit_rate
                    })
                }
                llm::StreamEvent::ContextCompacted {
                    before_tokens,
                    after_tokens,
                    summary,
                } => {
                    json!({
                        "type": "context_compacted",
                        "beforeTokens": before_tokens,
                        "afterTokens": after_tokens,
                        "summary": summary
                    })
                }
                llm::StreamEvent::TaskBoardUpdated { items } => {
                    json!({
                        "type": "task_board_updated",
                        "items": items
                    })
                }
                llm::StreamEvent::PlanApprovalRequired { plan_summary } => {
                    json!({
                        "type": "plan_approval_required",
                        "planSummary": plan_summary
                    })
                }
                llm::StreamEvent::GoalStatusUpdated {
                    status,
                    condition,
                    turn,
                    max_turns,
                    reason,
                    elapsed_secs,
                } => {
                    json!({
                        "type": "goal_status_updated",
                        "status": status,
                        "condition": condition,
                        "turn": turn,
                        "maxTurns": max_turns,
                        "reason": reason,
                        "elapsedSecs": elapsed_secs
                    })
                }
                llm::StreamEvent::Error { error } => {
                    json!({ "type": "error", "message": error })
                }
            };
            let _ = on_event_clone.send(val);
        }
    });

    let contract_mutex = {
        let mut map = state.active_contracts.write().await;
        map.entry(chat_id.clone())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(None)))
            .clone()
    };
    let stream_mgr = state.stream_mgr.clone();
    let db_arc = state.app_state.db.clone();
    let is_resume = resume.unwrap_or(false);
    let resumed_history: Option<Vec<llm::request_compat::ChatMessage>> = if is_resume {
        db_arc
            .load_checkpoint(&chat_id)
            .ok()
            .flatten()
            .and_then(|j| serde_json::from_str(&j).ok())
    } else {
        None
    };
    let chat_id_str = chat_id.clone();

    let initial_history: Vec<llm::request_compat::ChatMessage> = history
        .unwrap_or_default()
        .into_iter()
        .filter(|h| !h.content.trim().is_empty() && (h.role == "user" || h.role == "assistant"))
        .map(|h| {
            if h.role == "assistant" {
                llm::request_compat::ChatMessage::assistant(h.content)
            } else {
                llm::request_compat::ChatMessage::user(h.content)
            }
        })
        .collect();

    let api_proto = api_protocol.clone();
    tokio::spawn(async move {
        let mut contract_lock = contract_mutex.lock().await;
        let mut history = if let Some(rh) = resumed_history {
            rh
        } else {
            let mut h = Vec::new();
            if !initial_history.is_empty() {
                let sys_prompt = engine::prompt_builder::PromptBuilder::build_system_prompt(
                    &engine::role_permissions::AgentRole::Main,
                    &workspace_str,
                    None,
                    None,
                    Some(&model_id),
                );
                h.push(llm::request_compat::ChatMessage::system(sys_prompt));
                h.extend(initial_history);
            }
            h
        };

        let goal_cmd = engine::goal_loop::parse_goal_command(&prompt);
        let res = if let Some(engine::goal_loop::GoalCommandAction::Clear) = goal_cmd {
            let _ = db_arc.delete_goal_state(&chat_id_str);
            let _ = tx
                .send(llm::StreamEvent::GoalStatusUpdated {
                    status: "cleared".to_string(),
                    condition: String::new(),
                    turn: 0,
                    max_turns: 0,
                    reason: None,
                    elapsed_secs: 0,
                })
                .await;
            let _ = tx
                .send(llm::StreamEvent::ContentDelta {
                    delta: "Goal cleared.".to_string(),
                })
                .await;
            Ok(())
        } else if let Some(engine::goal_loop::GoalCommandAction::Status) = goal_cmd {
            let loaded = db_arc
                .load_goal_state(&chat_id_str)
                .ok()
                .flatten()
                .and_then(|s| serde_json::from_str::<engine::goal_loop::GoalState>(&s).ok());
            let msg = if let Some(g) = loaded {
                let _ = tx.send(g.to_stream_event()).await;
                format!(
                    "Goal status: **{}** (turn {}/{}, elapsed {}s)\nCondition: {}{}",
                    g.status.as_str(),
                    g.turn,
                    g.max_turns,
                    g.elapsed_secs(),
                    g.condition,
                    g.last_reason
                        .as_deref()
                        .map(|r| format!("\nLast evaluator reason: {}", r))
                        .unwrap_or_default()
                )
            } else {
                "No active goal in this session.".to_string()
            };
            let _ = tx
                .send(llm::StreamEvent::ContentDelta { delta: msg })
                .await;
            Ok(())
        } else if is_swarm {
            let swarm_prompt = prompt.strip_prefix("/swarm").unwrap_or(&prompt).trim();
            engine::debate_swarm::DebateSwarmRunner::run_swarm(
                &chat_id_str,
                swarm_prompt,
                &workspace_str,
                tools,
                &key_pool,
                &caps_reg,
                &provider_name,
                &base_url,
                &model_id,
                None,
                &session,
                &mut *contract_lock,
                tx.clone(),
            )
            .await
            .map(|_| ())
        } else {
            let mut active_goal: Option<engine::goal_loop::GoalState> = match goal_cmd {
                Some(engine::goal_loop::GoalCommandAction::Start { condition }) => {
                    let clean_cond = condition
                        .split_whitespace()
                        .filter(|t| !t.eq_ignore_ascii_case("/plan"))
                        .collect::<Vec<_>>()
                        .join(" ");
                    let g = engine::goal_loop::GoalState::new(&chat_id_str, &clean_cond);
                    if let Ok(j) = serde_json::to_string(&g) {
                        let _ = db_arc.save_goal_state(&chat_id_str, &j);
                    }
                    let _ = tx.send(g.to_stream_event()).await;
                    Some(g)
                }
                _ => {
                    let existing = db_arc
                        .load_goal_state(&chat_id_str)
                        .ok()
                        .flatten()
                        .and_then(|s| serde_json::from_str::<engine::goal_loop::GoalState>(&s).ok());
                    if let Some(mut g) = existing {
                        if g.status == engine::goal_loop::GoalStatus::Paused
                            || g.status == engine::goal_loop::GoalStatus::Idle
                        {
                            g.status = engine::goal_loop::GoalStatus::Active;
                            g.zero_tool_streak = 0;
                            if let Ok(j) = serde_json::to_string(&g) {
                                let _ = db_arc.save_goal_state(&chat_id_str, &j);
                            }
                            let _ = tx.send(g.to_stream_event()).await;
                            Some(g)
                        } else if g.status == engine::goal_loop::GoalStatus::Active {
                            Some(g)
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                }
            };

            let mut current_prompt = if let Some(ref g) = active_goal {
                if prompt.trim().starts_with("/goal") {
                    if tools.is_plan_mode() {
                        format!(
                            "GOAL + PLAN MODE ACTIVE.\nPhase 1 (Read-Only Plan): Explore the codebase, build a concrete step-by-step plan via task_board, then call exit_plan_mode(plan_summary) to unlock file edits.\nPhase 2 (Execute & Prove): Implement the plan and prove with actual tool outputs that the condition is satisfied:\n{}",
                            g.condition
                        )
                    } else {
                        format!(
                            "GOAL MODE ACTIVE (Autonomous Completion Loop).\nCondition to satisfy and prove with actual tool outputs:\n{}",
                            g.condition
                        )
                    }
                } else {
                    prompt.clone()
                }
            } else {
                prompt.clone()
            };

            let mut first_turn = true;
            loop {
                let hist_len_before = history.len();
                let turn_res = engine::single_agent::SingleAgentRunner::run_turn(
                    &http_client,
                    &chat_id_str,
                    &current_prompt,
                    &workspace_str,
                    tools.clone(),
                    &key_pool,
                    &caps_reg,
                    &provider_name,
                    &base_url,
                    api_proto.as_deref(),
                    &model_id,
                    &reasoning_effort,
                    None,
                    Some(&db_arc),
                    if first_turn { is_resume } else { false },
                    &session,
                    &mut *contract_lock,
                    &mut history,
                    tx.clone(),
                )
                .await
                .map(|_| ());
                first_turn = false;

                if let Err(err) = turn_res {
                    if let Some(ref mut g) = active_goal {
                        if engine::goal_loop::is_unrecoverable_goal_error(&err) {
                            let _ = db_arc.delete_goal_state(&chat_id_str);
                            g.status = engine::goal_loop::GoalStatus::Failed;
                            g.last_reason = Some(err.clone());
                            let _ = tx.send(g.to_stream_event()).await;
                            break Err(format!("Goal cleared after an unrecoverable error: {}", err));
                        } else if !session.is_active() {
                            g.status = engine::goal_loop::GoalStatus::Paused;
                            if let Ok(j) = serde_json::to_string(g) {
                                let _ = db_arc.save_goal_state(&chat_id_str, &j);
                            }
                            let _ = tx.send(g.to_stream_event()).await;
                        }
                    }
                    break Err(err);
                }

                let Some(ref mut g) = active_goal else {
                    break Ok(());
                };

                let still_active_in_db = db_arc
                    .load_goal_state(&chat_id_str)
                    .ok()
                    .flatten()
                    .and_then(|s| serde_json::from_str::<engine::goal_loop::GoalState>(&s).ok())
                    .map(|db_g| db_g.status == engine::goal_loop::GoalStatus::Active)
                    .unwrap_or(false);

                if !still_active_in_db || !session.is_active() {
                    let _ = tx
                        .send(llm::StreamEvent::ContentDelta {
                            delta: "\n\nGoal cleared · finishing current step.".to_string(),
                        })
                        .await;
                    break Ok(());
                }

                let used_tools_this_turn = history[hist_len_before..]
                    .iter()
                    .any(|m| m.role == "tool" || m.tool_calls.as_ref().map(|v| !v.is_empty()).unwrap_or(false));
                if used_tools_this_turn {
                    g.zero_tool_streak = 0;
                } else {
                    g.zero_tool_streak += 1;
                }

                if g.zero_tool_streak >= 2 {
                    g.status = engine::goal_loop::GoalStatus::Paused;
                    g.last_reason = Some("No tools executed for 2 consecutive turns (stagnation breaker).".to_string());
                    if let Ok(j) = serde_json::to_string(g) {
                        let _ = db_arc.save_goal_state(&chat_id_str, &j);
                    }
                    let _ = tx.send(g.to_stream_event()).await;
                    let _ = tx
                        .send(llm::StreamEvent::ContentDelta {
                            delta: "\n\n**Goal Paused (Stagnation Breaker):** Worker produced 2 consecutive turns without tool execution. Send a message to resume.".to_string(),
                        })
                        .await;
                    break Ok(());
                }

                let verdict = engine::goal_loop::evaluate_transcript(
                    &http_client,
                    &provider_name,
                    &base_url,
                    api_proto.as_deref(),
                    &key_pool,
                    &caps_reg,
                    &model_id,
                    &g.condition,
                    &history,
                )
                .await;

                match verdict {
                    Ok(engine::goal_loop::EvaluatorVerdict::Met { reason }) => {
                        g.status = engine::goal_loop::GoalStatus::Achieved;
                        g.last_reason = Some(reason.clone());
                        let _ = db_arc.delete_goal_state(&chat_id_str);
                        let _ = tx.send(g.to_stream_event()).await;
                        let _ = tx
                            .send(llm::StreamEvent::ContentDelta {
                                delta: format!("\n\n**Goal Achieved:** {}", reason),
                            })
                            .await;
                        break Ok(());
                    }
                    Ok(engine::goal_loop::EvaluatorVerdict::Impossible { reason }) => {
                        g.status = engine::goal_loop::GoalStatus::Failed;
                        g.last_reason = Some(reason.clone());
                        let _ = db_arc.delete_goal_state(&chat_id_str);
                        let _ = tx.send(g.to_stream_event()).await;
                        let _ = tx
                            .send(llm::StreamEvent::ContentDelta {
                                delta: format!("\n\n**Goal Failed (Impossible):** {}", reason),
                            })
                            .await;
                        break Ok(());
                    }
                    Ok(engine::goal_loop::EvaluatorVerdict::NotYetMet { reason }) => {
                        g.turn += 1;
                        g.last_reason = Some(reason.clone());
                        if g.turn > g.max_turns {
                            g.status = engine::goal_loop::GoalStatus::Paused;
                            if let Ok(j) = serde_json::to_string(g) {
                                let _ = db_arc.save_goal_state(&chat_id_str, &j);
                            }
                            let _ = tx.send(g.to_stream_event()).await;
                            let _ = tx
                                .send(llm::StreamEvent::ContentDelta {
                                    delta: format!("\n\n**Goal Paused:** Turn budget reached ({}/{}). Send a message to continue.", g.max_turns, g.max_turns),
                                })
                                .await;
                            break Ok(());
                        }
                        if let Ok(j) = serde_json::to_string(g) {
                            let _ = db_arc.save_goal_state(&chat_id_str, &j);
                        }
                        let _ = tx.send(g.to_stream_event()).await;
                        let _ = tx
                            .send(llm::StreamEvent::ContentDelta {
                                delta: format!("\n\n**Goal Evaluator (Turn {}/{}) — NOT YET MET:** {}\n\n", g.turn, g.max_turns, reason),
                            })
                            .await;
                        current_prompt = format!(
                            "[Goal Stop-Hook Feedback — NOT YET MET]\nCondition: {}\nEvaluator Reason: {}\n\nContinue working and produce concrete tool output proving the condition is met.",
                            g.condition, reason
                        );
                    }
                    Err(eval_err) => {
                        g.status = engine::goal_loop::GoalStatus::Paused;
                        g.last_reason = Some(format!("Evaluator error: {}", eval_err));
                        if let Ok(j) = serde_json::to_string(g) {
                            let _ = db_arc.save_goal_state(&chat_id_str, &j);
                        }
                        let _ = tx.send(g.to_stream_event()).await;
                        let _ = tx
                            .send(llm::StreamEvent::ContentDelta {
                                delta: format!("\n\n**Goal Paused (Evaluator error):** {}", eval_err),
                            })
                            .await;
                        break Ok(());
                    }
                }
            }
        };

        match res {
            Ok(()) => {
                let _ = tx
                    .send(llm::StreamEvent::Finish {
                        finish_reason: Some("stop".to_string()),
                        usage: None,
                    })
                    .await;
            }
            Err(err) => {
                let _ = tx.send(llm::StreamEvent::Error { error: err }).await;
            }
        }

        stream_mgr.remove_session(&chat_id_str).await;
    });

    Ok(session_id)
}


#[tauri::command]
async fn ask_side_question(
    state: State<'_, AppManagedState>,
    chat_id: String,
    question: String,
    model_id: String,
    provider_name: Option<String>,
    base_url: Option<String>,
    api_key: Option<String>,
    api_protocol: Option<String>,
    proxy_enabled: Option<bool>,
    proxy_url: Option<String>,
    context_window: Option<u32>,
    max_output_tokens: Option<u32>,
    history: Option<Vec<HistoryInputItem>>,
    on_event: Channel<serde_json::Value>,
) -> Result<(), String> {
    let key_pool = (*state.key_pool).clone();
    let caps_reg = (*state.caps_registry).clone();
    if context_window.is_some() || max_output_tokens.is_some() {
        caps_reg.update_model_caps(&model_id, context_window, max_output_tokens, None);
    }
    let http_client = build_http_client(proxy_enabled, proxy_url.as_deref(), Some(&state.http_client))?;

    let (mut resolved_provider, mut resolved_url) = ("OpenAI".to_string(), "https://api.openai.com/v1".to_string());
    if let Ok(model_rec) = state.app_state.db.get_model(&model_id) {
        if let Ok(prov_rec) = state.app_state.db.get_provider(&model_rec.provider_id) {
            resolved_provider = prov_rec.name;
            resolved_url = prov_rec.base_url;
        }
    }
    if let Some(pn) = provider_name {
        if !pn.trim().is_empty() {
            resolved_provider = pn;
        }
    }
    if let Some(bu) = base_url {
        if !bu.trim().is_empty() {
            resolved_url = bu;
        }
    }
    if let Some(ak) = api_key {
        if !ak.trim().is_empty() {
            key_pool.set_keys(&resolved_provider, vec![ak.trim().to_string()]);
        }
    }

    let mut msgs: Vec<llm::request_compat::ChatMessage> = vec![
        llm::request_compat::ChatMessage::system(
            "You are answering a quick ephemeral side question (/btw) about the current session. You have NO tools available (cannot read files, run commands, or search the web). Answer concisely using ONLY the conversation context provided below. If the answer is not in the context, say so directly.".to_string()
        )
    ];

    let checkpoint_msgs: Vec<llm::request_compat::ChatMessage> = state
        .app_state
        .db
        .load_checkpoint(&chat_id)
        .ok()
        .flatten()
        .and_then(|j| serde_json::from_str(&j).ok())
        .unwrap_or_default();

    let mut context_added = false;
    if !checkpoint_msgs.is_empty() {
        for m in checkpoint_msgs {
            if m.role == "system" {
                continue;
            }
            let text = m.content.as_text_lossy().trim().to_string();
            if m.role == "tool" {
                if !text.is_empty() {
                    let short: String = text.chars().take(1200).collect();
                    msgs.push(llm::request_compat::ChatMessage::assistant(format!("[Tool output]
{}", short)));
                    context_added = true;
                }
            } else if m.role == "assistant" {
                if !text.is_empty() {
                    msgs.push(llm::request_compat::ChatMessage::assistant(text));
                    context_added = true;
                }
            } else if m.role == "user" && !text.is_empty() {
                msgs.push(llm::request_compat::ChatMessage::user(text));
                context_added = true;
            }
        }
    }

    if !context_added {
        if let Some(hist) = history {
            for h in hist {
                if h.content.trim().is_empty() {
                    continue;
                }
                if h.role == "assistant" {
                    msgs.push(llm::request_compat::ChatMessage::assistant(h.content));
                } else if h.role == "user" {
                    msgs.push(llm::request_compat::ChatMessage::user(h.content));
                }
            }
        }
    }

    msgs.push(llm::request_compat::ChatMessage::user(question));

    let req = llm::request_compat::ChatRequest {
        model: model_id,
        messages: msgs,
        temperature: Some(0.3),
        top_p: None,
        max_tokens: None,
        max_completion_tokens: Some(4096),
        reasoning_effort: None,
        thinking_budget: None,
        tools: None,
        tool_choice: None,
        response_format: None,
        stream: true,
        prompt_cache_key: None,
    };

    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let on_ev = on_event.clone();
    let fwd = tokio::spawn(async move {
        while let Some(evt) = rx.recv().await {
            let val = match evt {
                llm::StreamEvent::ContentDelta { delta } => json!({ "type": "delta", "content": delta }),
                llm::StreamEvent::ReasoningDelta { delta } => json!({ "type": "reasoning_delta", "content": delta }),
                llm::StreamEvent::Finish { .. } => json!({ "type": "done" }),
                llm::StreamEvent::Error { error } => json!({ "type": "error", "message": error }),
                _ => continue,
            };
            let _ = on_ev.send(val);
        }
    });

    let res = llm::dispatch_stream_with_protocol(
        &http_client,
        &resolved_provider,
        &resolved_url,
        api_protocol.as_deref(),
        &key_pool,
        &caps_reg,
        req,
        tx,
    )
    .await;

    let _ = fwd.await;
    if let Err(e) = res {
        let _ = on_event.send(json!({ "type": "error", "message": e.to_string() }));
        return Err(e.to_string());
    }
    Ok(())
}


#[tauri::command]
async fn get_goal_state(
    state: State<'_, AppManagedState>,
    chat_id: String,
) -> Result<Option<engine::goal_loop::GoalState>, String> {
    let raw = state
        .app_state
        .db
        .load_goal_state(&chat_id)
        .map_err(|e| e.to_string())?;
    Ok(raw.and_then(|s| serde_json::from_str::<engine::goal_loop::GoalState>(&s).ok()))
}

#[tauri::command]
async fn clear_goal_state(
    state: State<'_, AppManagedState>,
    chat_id: String,
) -> Result<(), String> {
    state
        .app_state
        .db
        .delete_goal_state(&chat_id)
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn cancel_stream(
    state: State<'_, AppManagedState>,
    session_id: String,
) -> Result<(), String> {
    let _ = state.stream_mgr.cancel_session(&session_id).await;
    let _ = state.tool_dispatcher.pty_manager.kill_all_for_chat(&session_id);
    Ok(())
}

#[tauri::command]
async fn send_intervention(
    state: State<'_, AppManagedState>,
    session_id: String,
    message: String,
) -> Result<(), String> {
    state.stream_mgr.inject_intervention(&session_id, message).await
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DetectedModelInfo {
    pub id: String,
    pub name: String,
    pub context_window: u32,
    pub max_output_tokens: u32,
    pub supports_reasoning: bool,
    pub supports_vision: bool,
    pub supports_tools: bool,
    pub supports_audio: bool,
    pub supports_files: bool,
}

fn build_detected_model(id: &str, name: Option<&str>, raw: Option<&serde_json::Value>) -> DetectedModelInfo {
    let caps = llm::capabilities::ModelCapabilities::default_for_model(id);
    let lower = id.to_lowercase();
    let mut ctx = caps.context_window;
    let mut max_out = caps.max_output_tokens;
    let mut reasoning = caps.supports_reasoning;
    let mut vision = caps.supports_vision;
    let mut tools = caps.supports_tools;
    let mut audio = lower.contains("gpt-4o") || lower.contains("gemini") || lower.contains("audio");
    let mut files = lower.contains("claude") || lower.contains("gemini") || lower.contains("gpt-4") || lower.contains("gpt-5");

    let mut md_ctx_found = false;
    let mut md_out_found = false;
    if let Some(md) = llm::models_dev::lookup(id) {
        if md.context_window > 0 {
            ctx = md.context_window;
            md_ctx_found = true;
        }
        if md.max_output_tokens > 0 {
            max_out = md.max_output_tokens;
            md_out_found = true;
        }
        if let Some(r) = md.reasoning {
            reasoning = r;
        }
        if let Some(v) = md.vision {
            vision = v;
        }
        if let Some(a) = md.audio {
            audio = a;
        }
        if let Some(f) = md.files {
            files = f;
        }
        if let Some(t) = md.tool_call {
            tools = t;
        }
    }

    if let Some(obj) = raw.and_then(|v| v.as_object()) {
        if let Some(c) = obj.get("context_length")
            .or_else(|| obj.get("context_window"))
            .or_else(|| obj.get("max_context_length"))
            .or_else(|| obj.get("input_token_limit"))
            .or_else(|| obj.get("inputTokenLimit"))
            .or_else(|| obj.get("max_input_tokens"))
            .or_else(|| obj.get("limit").and_then(|v| v.get("context")))
            .or_else(|| obj.get("limits").and_then(|v| v.get("context")))
            .or_else(|| obj.get("top_provider").and_then(|v| v.get("context_length")))
            .and_then(|v| v.as_u64())
        {
            if c > 0 { ctx = if md_ctx_found { ctx.max(c as u32) } else { c as u32 }; }
        }
        if let Some(m) = obj.get("top_provider")
            .and_then(|v| v.get("max_completion_tokens"))
            .or_else(|| obj.get("max_output_tokens"))
            .or_else(|| obj.get("output_token_limit"))
            .or_else(|| obj.get("outputTokenLimit"))
            .or_else(|| obj.get("limit").and_then(|v| v.get("output")))
            .or_else(|| obj.get("limits").and_then(|v| v.get("output")))
            .and_then(|v| v.as_u64())
        {
            if m > 0 { max_out = if md_out_found { max_out.max(m as u32) } else { m as u32 }; }
        }
        if let Some(cap_obj) = obj.get("capabilities").and_then(|v| v.as_object()) {
            if let Some(t) = cap_obj.get("tools").and_then(|v| v.as_bool()) {
                tools = t;
            }
            if let Some(r) = cap_obj.get("reasoning").and_then(|v| v.as_bool()) {
                reasoning = r;
            }
            if let Some(inp) = cap_obj.get("input").and_then(|v| v.as_array()) {
                let mods: Vec<String> = inp.iter().filter_map(|m| m.as_str().map(|s| s.to_lowercase())).collect();
                vision = mods.iter().any(|m| m == "image");
                audio = mods.iter().any(|m| m == "audio");
                files = mods.iter().any(|m| m == "file" || m == "pdf");
            }
        }
        if let Some(r_obj) = obj.get("reasoning").and_then(|v| v.as_object()) {
            let has_efforts = r_obj.get("supported_efforts").and_then(|v| v.as_array()).map(|a| !a.is_empty()).unwrap_or(false);
            let mandatory = r_obj.get("mandatory").and_then(|v| v.as_bool()).unwrap_or(false);
            let def_en = r_obj.get("default_enabled").and_then(|v| v.as_bool()).unwrap_or(false);
            if has_efforts || mandatory || def_en {
                reasoning = true;
            }
        }
        if let Some(params) = obj.get("supported_parameters").and_then(|v| v.as_array()) {
            let pstrs: Vec<String> = params.iter().filter_map(|p| p.as_str().map(|s| s.to_lowercase())).collect();
            if pstrs.iter().any(|p| p.contains("reasoning") || p.contains("thinking")) {
                reasoning = true;
            }
            if pstrs.iter().any(|p| p.contains("tools")) {
                tools = true;
            } else if !pstrs.is_empty() {
                tools = false;
            }
        }
        if let Some(arch) = obj.get("architecture").and_then(|v| v.get("input_modalities")).and_then(|v| v.as_array()) {
            let mods: Vec<String> = arch.iter().filter_map(|m| m.as_str().map(|s| s.to_lowercase())).collect();
            vision = mods.iter().any(|m| m == "image");
            audio = mods.iter().any(|m| m == "audio");
            files = mods.iter().any(|m| m == "file");
        }
    }

    DetectedModelInfo {
        id: id.to_string(),
        name: name.unwrap_or(id).to_string(),
        context_window: ctx,
        max_output_tokens: max_out,
        supports_reasoning: reasoning,
        supports_vision: vision,
        supports_tools: tools,
        supports_audio: audio,
        supports_files: files,
    }
}

#[tauri::command]
async fn detect_model_capabilities(model_id: String) -> Result<DetectedModelInfo, String> {
    llm::models_dev::ensure_loaded().await;
    Ok(build_detected_model(&model_id, None, None))
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelTestResult {
    pub ok: bool,
    pub latency_ms: u64,
    pub error: Option<String>,
}

#[tauri::command]
async fn test_provider_model(
    base_url: String,
    api_key: String,
    model_id: String,
    api_protocol: Option<String>,
    proxy_enabled: Option<bool>,
    proxy_url: Option<String>,
) -> Result<ModelTestResult, String> {
    let started = std::time::Instant::now();
    let client = build_http_client(proxy_enabled, proxy_url.as_deref(), None)?;
    let api_key = llm::KeyPool::parse_api_keys(&api_key).into_iter().next().unwrap_or_default();
    let clean_base = base_url.trim_end_matches('/');
    let proto = api_protocol.unwrap_or_else(|| "auto".to_string()).to_lowercase();
    let bl = clean_base.to_lowercase();
    let is_google = proto == "google_gemini" || (proto == "auto" && bl.contains("generativelanguage.googleapis.com") && !bl.contains("/openai"));
    let is_anth = proto == "anthropic_messages" || (proto == "auto" && bl.contains("anthropic"));
    let is_resp = proto == "responses" || (proto == "auto" && clean_base.ends_with("/responses"));

    let req = if is_google {
        let m = model_id.strip_prefix("models/").unwrap_or(&model_id);
        let url = format!("{}/models/{}:generateContent", clean_base, m);
        client
            .post(&url)
            .header("x-goog-api-key", api_key.trim())
            .json(&json!({
                "contents": [{ "role": "user", "parts": [{ "text": "ping" }] }],
                "generationConfig": { "maxOutputTokens": 1 }
            }))
    } else if is_anth {
        let url = if clean_base.ends_with("/messages") { clean_base.to_string() } else { format!("{}/messages", clean_base) };
        client
            .post(&url)
            .header("x-api-key", api_key.trim())
            .header("anthropic-version", "2023-06-01")
            .json(&json!({
                "model": model_id,
                "max_tokens": 1,
                "messages": [{ "role": "user", "content": "ping" }]
            }))
    } else if is_resp {
        let url = if clean_base.ends_with("/responses") { clean_base.to_string() } else { format!("{}/responses", clean_base) };
        client
            .post(&url)
            .bearer_auth(api_key.trim())
            .json(&json!({
                "model": model_id,
                "input": "ping",
                "max_output_tokens": 1
            }))
    } else {
        let url = if clean_base.ends_with("/chat/completions") { clean_base.to_string() } else { format!("{}/chat/completions", clean_base) };
        client
            .post(&url)
            .bearer_auth(api_key.trim())
            .json(&json!({
                "model": model_id,
                "messages": [{ "role": "user", "content": "ping" }],
                "max_tokens": 1
            }))
    };

    match req.timeout(std::time::Duration::from_secs(15)).send().await {
        Ok(resp) => {
            let ms = started.elapsed().as_millis() as u64;
            if resp.status().is_success() {
                Ok(ModelTestResult { ok: true, latency_ms: ms, error: None })
            } else {
                let code = resp.status().as_u16();
                let body = resp.text().await.unwrap_or_default();
                Ok(ModelTestResult { ok: false, latency_ms: ms, error: Some(format!("HTTP {}: {}", code, body.chars().take(180).collect::<String>())) })
            }
        }
        Err(e) => Ok(ModelTestResult {
            ok: false,
            latency_ms: started.elapsed().as_millis() as u64,
            error: Some(e.to_string()),
        }),
    }
}

#[tauri::command]
async fn fetch_provider_models(
    base_url: String,
    api_key: String,
    proxy_enabled: Option<bool>,
    proxy_url: Option<String>,
) -> Result<Vec<DetectedModelInfo>, String> {
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let api_key = llm::KeyPool::parse_api_keys(&api_key).into_iter().next().unwrap_or_default();
    let client = build_http_client(proxy_enabled, proxy_url.as_deref(), None)?;

    let mut req = client.get(&url);
    if !api_key.trim().is_empty() {
        let bl = base_url.to_lowercase();
        if bl.contains("anthropic") {
            req = req.header("x-api-key", api_key.trim()).header("anthropic-version", "2023-06-01");
        } else if bl.contains("generativelanguage.googleapis.com") && !bl.contains("/openai") {
            req = req.header("x-goog-api-key", api_key.trim());
        } else {
            req = req.bearer_auth(api_key.trim());
        }
    }

    let resp = req.send().await.map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {} from {}", resp.status(), url));
    }

    let json_val: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
    llm::models_dev::ensure_loaded().await;
    let list = json_val.get("data").and_then(|v| v.as_array())
        .or_else(|| json_val.get("models").and_then(|v| v.as_array()))
        .or_else(|| json_val.as_array())
        .ok_or_else(|| "Invalid /models response format".to_string())?;

    let mut out = Vec::new();
    for item in list {
        let raw_id = item.get("id").and_then(|v| v.as_str())
            .or_else(|| item.get("name").and_then(|v| v.as_str()).map(|s| s.strip_prefix("models/").unwrap_or(s)));
        if let Some(id) = raw_id {
            let name = item.get("displayName").and_then(|v| v.as_str())
                .or_else(|| item.get("name").and_then(|v| v.as_str()));
            out.push(build_detected_model(id, name, Some(item)));
        }
    }
    Ok(out)
}

#[tauri::command]
async fn get_default_install_dir() -> Result<String, String> {
    if let Ok(local_app) = std::env::var("LOCALAPPDATA") {
        return Ok(PathBuf::from(local_app).join("OmniHarness").to_string_lossy().to_string());
    }
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."));
    Ok(exe_dir.to_string_lossy().to_string())
}

static EMBEDDED_WEBVIEW2_DLL: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/WebView2Loader.dll"));

#[tauri::command]
async fn install_app_to_dir(install_dir: String) -> Result<String, String> {
    let target_dir = PathBuf::from(install_dir.trim());
    std::fs::create_dir_all(&target_dir).map_err(|e| e.to_string())?;
    let current_exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let exe_name = current_exe
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("omni-harness.exe"));
    let target_exe = target_dir.join(exe_name);
    if current_exe != target_exe {
        let _ = std::fs::copy(&current_exe, &target_exe);
        if let Some(src_dir) = current_exe.parent() {
            if let Ok(entries) = std::fs::read_dir(src_dir) {
                for entry in entries.flatten() {
                    let p = entry.path();
                    if p.is_file() {
                        if let Some(ext) = p.extension().and_then(|e| e.to_str()) {
                            if ext.eq_ignore_ascii_case("dll") {
                                if let Some(fname) = p.file_name() {
                                    let _ = std::fs::copy(&p, target_dir.join(fname));
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    let wv2_dest = target_dir.join("WebView2Loader.dll");
    if !wv2_dest.exists() && !EMBEDDED_WEBVIEW2_DLL.is_empty() {
        let _ = std::fs::write(&wv2_dest, EMBEDDED_WEBVIEW2_DLL);
    }
    Ok(target_exe.to_string_lossy().to_string())
}

#[tauri::command]
async fn set_windows_autostart(enabled: bool, target_path: Option<String>) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        if enabled {
            let exe_path = match target_path {
                Some(p) if !p.trim().is_empty() => p,
                _ => std::env::current_exe().map_err(|e| e.to_string())?.to_string_lossy().to_string(),
            };
            let quoted = format!("\"{}\"", exe_path);
            let _ = std::process::Command::new("reg")
                .args(["add", r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run", "/v", "OmniHarness", "/t", "REG_SZ", "/d", &quoted, "/f"])
                .creation_flags(0x08000000)
                .output();
        } else {
            let _ = std::process::Command::new("reg")
                .args(["delete", r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run", "/v", "OmniHarness", "/f"])
                .creation_flags(0x08000000)
                .output();
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (enabled, target_path);
    }
    Ok(())
}

#[tauri::command]
async fn create_desktop_shortcut(target_path: Option<String>) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        let exe_path = match target_path {
            Some(p) if !p.trim().is_empty() => PathBuf::from(p),
            _ => std::env::current_exe().map_err(|e| e.to_string())?,
        };
        let exe_str = exe_path.to_string_lossy().replace("'", "''");
        let work_dir = exe_path
            .parent()
            .map(|p| p.to_string_lossy().replace("'", "''"))
            .unwrap_or_default();
        let ps = format!(
            "$ws = New-Object -ComObject WScript.Shell; $s = $ws.CreateShortcut((Join-Path ([Environment]::GetFolderPath('Desktop')) 'OmniHarness.lnk')); $s.TargetPath = '{}'; $s.WorkingDirectory = '{}'; $s.IconLocation = '{}'; $s.Save()",
            exe_str, work_dir, exe_str
        );
        std::process::Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-WindowStyle", "Hidden", "-Command", &ps])
            .creation_flags(0x08000000)
            .output()
            .map_err(|e| e.to_string())?;
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = target_path;
    }
    Ok(())
}

#[tauri::command]
async fn open_path_in_explorer(path: String) -> Result<(), String> {
    let raw = path.trim();
    let stripped = raw
        .strip_prefix("file:///")
        .or_else(|| raw.strip_prefix("file://"))
        .unwrap_or(raw);
    let decoded = urlencoding::decode(stripped)
        .map(|c| c.into_owned())
        .unwrap_or_else(|_| stripped.replace("%20", " "));
    let mut normalized = decoded.replace('/', "\\");
    if let Some(rest) = normalized.strip_prefix(r"\\?\") {
        normalized = rest.to_string();
    }
    let p = std::path::Path::new(&normalized);
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        if p.is_file() {
            std::process::Command::new("explorer.exe")
                .raw_arg(format!("/select,\"{}\"", normalized))
                .spawn()
                .map_err(|e| e.to_string())?;
        } else if p.is_dir() {
            std::process::Command::new("explorer.exe")
                .raw_arg(format!("\"{}\"", normalized))
                .spawn()
                .map_err(|e| e.to_string())?;
        } else if let Some(parent) = p.parent().filter(|pp| pp.exists()) {
            std::process::Command::new("explorer.exe")
                .raw_arg(format!("\"{}\"", parent.to_string_lossy()))
                .spawn()
                .map_err(|e| e.to_string())?;
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = p;
    }
    Ok(())
}

#[tauri::command]
async fn oauth_list_accounts(provider: Option<String>) -> Result<Vec<llm::oauth_accounts::OAuthAccountPublic>, String> {
    Ok(llm::oauth_accounts::list_accounts(provider.as_deref()).await)
}

#[tauri::command]
async fn oauth_start_login(
    app: tauri::AppHandle,
    provider: String,
    proxy_enabled: Option<bool>,
    proxy_url: Option<String>,
) -> Result<serde_json::Value, String> {
    let client = build_http_client(proxy_enabled, proxy_url.as_deref(), None)?;
    llm::oauth_accounts::start_oauth_login(app, client, &provider).await
}

#[tauri::command]
async fn oauth_complete_manual_login(
    app: tauri::AppHandle,
    provider: String,
    code_or_url: String,
    proxy_enabled: Option<bool>,
    proxy_url: Option<String>,
) -> Result<llm::oauth_accounts::OAuthAccountPublic, String> {
    let client = build_http_client(proxy_enabled, proxy_url.as_deref(), None)?;
    llm::oauth_accounts::complete_manual_oauth(app, client, &provider, &code_or_url).await
}

#[tauri::command]
async fn oauth_import_local_accounts(
    app: tauri::AppHandle,
    proxy_enabled: Option<bool>,
    proxy_url: Option<String>,
) -> Result<Vec<llm::oauth_accounts::OAuthAccountPublic>, String> {
    let client = build_http_client(proxy_enabled, proxy_url.as_deref(), None)?;
    llm::oauth_accounts::import_local_accounts(app, client).await
}

#[tauri::command]
async fn oauth_toggle_account(id: String, enabled: bool) -> Result<(), String> {
    llm::oauth_accounts::toggle_account(&id, enabled).await
}

#[tauri::command]
async fn oauth_delete_account(id: String) -> Result<(), String> {
    llm::oauth_accounts::delete_account(&id).await
}

#[tauri::command]
async fn oauth_fetch_antigravity_models(
    proxy_enabled: Option<bool>,
    proxy_url: Option<String>,
) -> Result<Vec<serde_json::Value>, String> {
    let client = build_http_client(proxy_enabled, proxy_url.as_deref(), None)?;
    llm::oauth_accounts::fetch_antigravity_models(&client).await
}

#[tauri::command]
async fn oauth_fetch_codex_models(
    proxy_enabled: Option<bool>,
    proxy_url: Option<String>,
) -> Result<Vec<serde_json::Value>, String> {
    let client = build_http_client(proxy_enabled, proxy_url.as_deref(), None)?;
    llm::oauth_accounts::fetch_codex_models(&client).await
}

#[tauri::command]
async fn oauth_fetch_claude_models(
    proxy_enabled: Option<bool>,
    proxy_url: Option<String>,
) -> Result<Vec<serde_json::Value>, String> {
    let client = build_http_client(proxy_enabled, proxy_url.as_deref(), None)?;
    llm::oauth_accounts::fetch_claude_models(&client).await
}

#[tauri::command]
async fn list_subagents(
    state: State<'_, AppManagedState>,
) -> Result<Vec<engine::subagents::SubagentInfo>, String> {
    let ws = state.app_state.get_workspace_dir().await;
    Ok(engine::subagents::list_all_subagents(&ws.to_string_lossy()))
}

#[tauri::command]
async fn save_custom_subagent(
    state: State<'_, AppManagedState>,
    name: String,
    description: String,
    tools: Vec<String>,
    disallowed_tools: Vec<String>,
    model: Option<String>,
    max_turns: Option<u32>,
    system_prompt: String,
    scope: String,
) -> Result<engine::subagents::SubagentInfo, String> {
    let ws = state.app_state.get_workspace_dir().await;
    let def = engine::subagents::CustomSubagentDef {
        name,
        description,
        tools,
        disallowed_tools,
        model,
        max_turns,
        system_prompt,
    };
    engine::subagents::save_custom_subagent(&ws.to_string_lossy(), def, &scope)
}

#[tauri::command]
async fn delete_custom_subagent(
    state: State<'_, AppManagedState>,
    name: String,
    file_path: Option<String>,
) -> Result<(), String> {
    let ws = state.app_state.get_workspace_dir().await;
    engine::subagents::delete_custom_subagent(&ws.to_string_lossy(), &name, file_path.as_deref())
}

#[tauri::command]
async fn list_skills(
    state: State<'_, AppManagedState>,
) -> Result<Vec<skills::SkillPackage>, String> {
    let ws = state.app_state.get_workspace_dir().await;
    Ok(skills::discover_all_skills(&ws.to_string_lossy()).into_iter().filter(|sk| !sk.metadata.name.starts_with("system:")).collect())
}

#[tauri::command]
async fn delete_skill(
    state: State<'_, AppManagedState>,
    skill_path: String,
    skill_name: String,
) -> Result<(), String> {
    let ws = state.app_state.get_workspace_dir().await;
    let ws_str = ws.to_string_lossy().to_string();
    let target = if !skill_path.trim().is_empty() {
        PathBuf::from(skill_path.trim())
    } else {
        ws.join(".omni").join("skills").join(skill_name.trim())
    };
    let canon_target = std::fs::canonicalize(&target).map_err(|e| format!("Skill path not found: {}", e))?;
    let allowed_roots = skills::skill_search_roots(&ws_str);
    let mut is_allowed = false;
    for r in allowed_roots {
        if let Ok(canon_root) = std::fs::canonicalize(&r) {
            if canon_target.starts_with(&canon_root) && canon_target != canon_root {
                is_allowed = true;
                break;
            }
        }
    }
    if !is_allowed || !canon_target.join("SKILL.md").exists() {
        return Err("Refusing to delete path outside registered skills directories".to_string());
    }
    std::fs::remove_dir_all(&canon_target).map_err(|e| e.to_string())
}

#[tauri::command]
async fn import_skill_zip(state: State<'_, AppManagedState>, path: String) -> Result<(), String> {
    let _ = &state;
    let skills_dir = state::AppState::resolve_skills_dir();
    let bytes = std::fs::read(&path).map_err(|e| e.to_string())?;
    skills::import_skill_zip(&bytes, &skills_dir).map(|_| ())
}

#[tauri::command]
async fn export_skill_zip(
    state: State<'_, AppManagedState>,
    skill_name: String,
    destination: String,
) -> Result<(), String> {
    let _ = &state;
    let skills_dir = state::AppState::resolve_skills_dir();
    let bytes = skills::export_skill_zip(&skill_name, &skills_dir)?;
    std::fs::write(&destination, bytes).map_err(|e| e.to_string())
}

#[tauri::command]
async fn transcribe_audio(
    state: State<'_, AppManagedState>,
    audio_data: Vec<u8>,
) -> Result<String, String> {
    let fallback_key = state.app_state.get_credential("openai_api_key").await.ok();
    let res = state
        .speech_engine
        .transcribe_audio(&audio_data, fallback_key.as_deref())
        .await?;
    Ok(res.text)
}

#[tauri::command]
async fn get_speech_status(
    state: State<'_, AppManagedState>,
) -> Result<Vec<speech::WhisperModelInfo>, String> {
    Ok(state.speech_engine.get_available_models())
}

#[tauri::command]
async fn download_whisper_model(
    state: State<'_, AppManagedState>,
    model_id: String,
) -> Result<String, String> {
    let path = state
        .speech_engine
        .download_model(&model_id, None)
        .await?;
    Ok(path.to_string_lossy().to_string())
}


#[tauri::command]
async fn sync_providers_to_backend(
    state: State<'_, AppManagedState>,
    providers: Vec<telegram::SyncedProviderConfig>,
    active_provider: String,
    active_model: String,
) -> Result<(), String> {
    let snap = telegram::ProvidersSnapshotFile {
        providers,
        active_provider,
        active_model,
    };
    state.telegram_mgr.sync_providers(snap).await
}

#[tauri::command]
async fn remove_telegram_paired_chat(
    state: State<'_, AppManagedState>,
    chat_id: String,
) -> Result<(), String> {
    state.telegram_mgr.remove_paired_chat(&chat_id).await
}

#[tauri::command]
async fn get_telegram_config(
    state: State<'_, AppManagedState>,
) -> Result<serde_json::Value, String> {
    let cfg = state.telegram_mgr.get_config_snapshot().await;
    let token = state.telegram_mgr.get_token().await.unwrap_or(cfg.token);
    let chats = state.telegram_mgr.get_allowed_chats().await;
    let running = state.telegram_mgr.is_running().await;
    let code = state.telegram_mgr.get_or_generate_pairing_code().await;

    Ok(json!({
        "botToken": token,
        "pairingCode": code,
        "pairedChats": cfg.paired_chats,
        "allowedChatIds": chats,
        "isRunning": running,
        "notifyOnComplete": cfg.notify_on_complete,
        "notifyOnApproval": cfg.notify_on_approval,
        "notifyOnError": cfg.notify_on_error,
        "autoStart": cfg.auto_start,
        "proxyUrl": cfg.proxy_url,
    }))
}

#[tauri::command]
async fn start_telegram_bot(
    state: State<'_, AppManagedState>,
    token: String,
    allowed_chats: Option<Vec<String>>,
) -> Result<(), String> {
    let clean = token.trim().to_string();
    if clean.is_empty() {
        return Err("Bot token cannot be empty".to_string());
    }
    state.telegram_mgr.set_token(clean).await;
    if let Some(list) = allowed_chats {
        for raw in list {
            if let Ok(id) = raw.trim().parse::<i64>() {
                state.telegram_mgr.add_allowed_chat(id).await;
            }
        }
    }
    state.telegram_mgr.start().await
}

#[tauri::command]
async fn stop_telegram_bot(state: State<'_, AppManagedState>) -> Result<(), String> {
    state.telegram_mgr.stop().await;
    Ok(())
}

#[tauri::command]
async fn save_telegram_config(
    state: State<'_, AppManagedState>,
    token: String,
    paired_chats: Vec<telegram::PairedChatEntry>,
    notify_on_complete: bool,
    notify_on_approval: bool,
    notify_on_error: bool,
    auto_start: Option<bool>,
    proxy_url: Option<String>,
) -> Result<(), String> {
    let prev = state.telegram_mgr.get_config_snapshot().await;
    let cfg = telegram::TelegramConfigFile {
        token: token.trim().to_string(),
        paired_chats,
        notify_on_complete,
        notify_on_approval,
        notify_on_error,
        auto_start: auto_start.unwrap_or(prev.auto_start),
        proxy_url: proxy_url.map(|s| s.trim().to_string()).unwrap_or_else(|| prev.proxy_url.clone()),
        offset: prev.offset,
        chat_sessions: prev.chat_sessions,
        chat_providers: prev.chat_providers,
        chat_models: prev.chat_models,
        chat_reasoning: prev.chat_reasoning,
        ..prev
    };
    state.telegram_mgr.save_config(cfg).await
}

#[tauri::command]
async fn set_telegram_running(state: State<'_, AppManagedState>, run: bool) -> Result<(), String> {
    if run {
        state.telegram_mgr.start().await
    } else {
        state.telegram_mgr.stop().await;
        Ok(())
    }
}

#[tauri::command]
async fn generate_pairing_code(state: State<'_, AppManagedState>) -> Result<String, String> {
    Ok(state.telegram_mgr.generate_pairing_code().await)
}

#[tauri::command]
async fn verify_pairing_code(
    state: State<'_, AppManagedState>,
    code: String,
    chat_id: i64,
) -> Result<bool, String> {
    Ok(state.telegram_mgr.verify_pairing_code(&code, chat_id).await)
}

pub fn run() {
    let app_state_inner = state::AppState::new().expect("Failed to initialize AppState");
    let data_dir = app_state_inner.data_dir.clone();
    let initial_ws = state::AppState::resolve_install_workspace();

    let speech_eng = Arc::new(speech::SpeechEngine::new(&data_dir));
    let stream_m = Arc::new(engine::StreamManager::new());
    let mcp_cat = Arc::new(mcp::McpServerCatalog::new());
    let tele_m = Arc::new(telegram::TelegramBotManager::new());
    let kp = Arc::new(llm::KeyPool::new());
    let cr = Arc::new(llm::CapabilityRegistry::new());
    let chat_tmp_root = state::AppState::resolve_chat_temp_dir(None);
    let td = Arc::new(tools::ToolDispatcher::new(vec![initial_ws, chat_tmp_root], None));
    td.set_db(app_state_inner.db.clone());
    let shared_http = reqwest::Client::builder()
        .pool_max_idle_per_host(8)
        .connect_timeout(std::time::Duration::from_secs(15))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());

    let app_state_arc = Arc::new(app_state_inner);
    let active_contracts_arc = Arc::new(RwLock::new(std::collections::HashMap::new()));
    let tg_app_handle = Arc::new(RwLock::new(None));
    let tg_app_handle_setup = tg_app_handle.clone();
    let tg_ctx = telegram::TelegramRuntimeContext {
        app_handle: tg_app_handle,
        app_state: app_state_arc.clone(),
        stream_mgr: stream_m.clone(),
        tool_dispatcher: td.clone(),
        key_pool: kp.clone(),
        caps_registry: cr.clone(),
        http_client: shared_http.clone(),
        active_contracts: active_contracts_arc.clone(),
    };
    let md_dir = data_dir.clone();
    tauri::async_runtime::spawn(async move {
        llm::models_dev::init_and_refresh(md_dir).await;
    });
    tauri::async_runtime::block_on(async {
        tele_m.load_from_disk(&data_dir).await;
        tele_m.set_runtime_context(tg_ctx).await;
    });

    let managed = AppManagedState {
        app_state: app_state_arc,
        stream_mgr: stream_m,
        mcp_catalog: mcp_cat,
        telegram_mgr: tele_m,
        speech_engine: speech_eng,
        tool_dispatcher: td,
        key_pool: kp,
        caps_registry: cr,
        http_client: shared_http,
        active_contracts: active_contracts_arc,
    };

    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        .plugin(tauri_plugin_notification::init())
        .manage(managed)
        .setup(move |app| {
            let h = app.handle().clone();
            let target = tg_app_handle_setup.clone();
            let tg_mgr_boot = h.state::<AppManagedState>().telegram_mgr.clone();
            tauri::async_runtime::spawn(async move {
                *target.write().await = Some(h);
                let cfg = tg_mgr_boot.get_config_snapshot().await;
                if cfg.auto_start && !cfg.token.trim().is_empty() {
                    let _ = tg_mgr_boot.start().await;
                }
            });
            let quit_i = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            let show_i = MenuItem::with_id(app, "show", "Show OmniHarness", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show_i, &quit_i])?;

            let mut tray_builder = TrayIconBuilder::new().menu(&menu);
            if let Some(icon) = app.default_window_icon() {
                tray_builder = tray_builder.icon(icon.clone());
            }
            let _tray = tray_builder
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "quit" => {
                        let st = app.state::<AppManagedState>();
                        let _ = st.tool_dispatcher.pty_manager.kill_all();
                        app.exit(0);
                    }
                    "show" => {
                        if let Some(win) = app.get_webview_window("main") {
                            let _ = win.show();
                            let _ = win.unminimize();
                            let _ = win.set_focus();
                        }
                    }
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let tauri::tray::TrayIconEvent::Click {
                        button: tauri::tray::MouseButton::Left,
                        button_state: tauri::tray::MouseButtonState::Up,
                        ..
                    } = event
                    {
                        if let Some(win) = tray.app_handle().get_webview_window("main") {
                            let _ = win.show();
                            let _ = win.unminimize();
                            let _ = win.set_focus();
                        }
                    }
                })
                .build(app)?;

            if let (Some(win), Some(icon)) = (app.get_webview_window("main"), app.default_window_icon()) {
                let _ = win.set_icon(icon.clone());
            }

            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                let app = window.app_handle();
                let state = app.state::<AppManagedState>();
                if state.has_active_tasks() {
                    api.prevent_close();
                    let _ = window.hide();
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            window_show,
            window_minimize,
            window_toggle_maximize,
            window_toggle_fullscreen,
            window_close,
            window_start_dragging,
            create_project,
            list_projects,
            get_project,
            delete_project,
            create_chat,
            list_chats,
            get_chat,
            update_chat_title,
            update_chat_pinned,
            delete_chat,
            read_file_full,
            create_message,
            list_messages,
            delete_messages_since,
            delete_message,
            record_file_snapshot,
            get_file_snapshot,
            list_snapshots_for_chat,
            rollback_file_snapshot,
            get_turn_file_diffs,
            get_events_since,
            list_session_checkpoints,
            rewind_checkpoint,
            record_usage_stat,
            list_usage_stats,
            create_provider,
            list_providers,
            get_provider,
            update_provider,
            delete_provider,
            ping_provider,
            create_model,
            list_models,
            get_model,
            update_model,
            delete_model,
            create_mcp_server,
            list_mcp_servers,
            get_mcp_server,
            update_mcp_server,
            delete_mcp_server,
            connect_mcp_server,
            disconnect_mcp_server,
            list_mcp_tools,
            import_mcp_config,
            call_mcp_tool,
            create_memory_entry,
            list_memory_entries,
            get_memory_entry,
            update_memory_entry,
            delete_memory_entry,
            search_memory_entries,
            get_total_memory_tokens,
            sync_chat_context,
            get_bootstrap_config,
            complete_first_run,
            get_workspace_dir,
            set_workspace_dir,
            get_credential,
            set_credential,
            delete_credential,
            start_chat_stream,
            ask_side_question,
            cancel_stream,
            get_goal_state,
            clear_goal_state,
            send_intervention,
            fetch_provider_models,
            test_provider_model,
            detect_model_capabilities,
            open_path_in_explorer,
            get_default_install_dir,
            install_app_to_dir,
            set_windows_autostart,
            create_desktop_shortcut,
            oauth_list_accounts,
            oauth_start_login,
            oauth_complete_manual_login,
            oauth_import_local_accounts,
            oauth_toggle_account,
            oauth_delete_account,
            oauth_fetch_antigravity_models,
            oauth_fetch_codex_models,
            oauth_fetch_claude_models,
            list_subagents,
            save_custom_subagent,
            delete_custom_subagent,
            list_skills,
            delete_skill,
            import_skill_zip,
            export_skill_zip,
            transcribe_audio,
            get_speech_status,
            download_whisper_model,
            get_telegram_config,
            start_telegram_bot,
            stop_telegram_bot,
            save_telegram_config,
            remove_telegram_paired_chat,
            sync_providers_to_backend,
            sync_workspaces_to_backend,
            set_telegram_running,
            generate_pairing_code,
            verify_pairing_code,
        ])
        .run(tauri::generate_context!())
        .expect("Error while running OmniHarness application");
}
