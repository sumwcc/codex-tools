use base64::engine::general_purpose::URL_SAFE;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use std::collections::HashSet;
use std::env;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;
use tauri::AppHandle;
use tauri::Manager;
use tauri::State;
use tokio::sync::Mutex;
use uuid::Uuid;

const DEFAULT_CHATGPT_BASE_URL: &str = "https://chatgpt.com/backend-api";
const CODEX_USAGE_PATH: &str = "/api/codex/usage";
const WHAM_USAGE_PATH: &str = "/wham/usage";
const REFRESH_INTERVAL_SECONDS: u64 = 30;

#[cfg(target_os = "macos")]
const TRAY_ID: &str = "codex_tools_status_bar";
#[cfg(target_os = "macos")]
const TRAY_MENU_REFRESH_ID: &str = "tray_refresh_usage";
#[cfg(target_os = "macos")]
const TRAY_MENU_OPEN_ID: &str = "tray_open_window";
#[cfg(target_os = "macos")]
const TRAY_MENU_QUIT_ID: &str = "tray_quit";
#[cfg(target_os = "macos")]
const STATUS_BAR_ICON: tauri::image::Image<'_> =
    tauri::include_image!("./icons/codex-tools-statusbar-terminal.png");

#[derive(Default)]
struct AppState {
    store_lock: Mutex<()>,
    add_flow_auth_backup: Mutex<Option<Option<Value>>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct AccountsStore {
    version: u8,
    accounts: Vec<StoredAccount>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredAccount {
    id: String,
    label: String,
    email: Option<String>,
    account_id: String,
    plan_type: Option<String>,
    auth_json: Value,
    added_at: i64,
    updated_at: i64,
    usage: Option<UsageSnapshot>,
    usage_error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct AccountSummary {
    id: String,
    label: String,
    email: Option<String>,
    account_id: String,
    plan_type: Option<String>,
    added_at: i64,
    updated_at: i64,
    usage: Option<UsageSnapshot>,
    usage_error: Option<String>,
    is_current: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UsageSnapshot {
    fetched_at: i64,
    plan_type: Option<String>,
    five_hour: Option<UsageWindow>,
    one_week: Option<UsageWindow>,
    credits: Option<CreditSnapshot>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UsageWindow {
    used_percent: f64,
    window_seconds: i64,
    reset_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreditSnapshot {
    has_credits: bool,
    unlimited: bool,
    balance: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct SwitchAccountResult {
    account_id: String,
    launched_app_path: Option<String>,
    used_fallback_cli: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct CurrentAuthStatus {
    available: bool,
    account_id: Option<String>,
    email: Option<String>,
    plan_type: Option<String>,
    auth_mode: Option<String>,
    last_refresh: Option<String>,
    file_modified_at: Option<i64>,
    fingerprint: Option<String>,
}

#[derive(Debug, Deserialize)]
struct UsageApiResponse {
    plan_type: Option<String>,
    rate_limit: Option<RateLimitDetails>,
    additional_rate_limits: Option<Vec<AdditionalRateLimitDetails>>,
    credits: Option<CreditDetails>,
}

#[derive(Debug, Deserialize)]
struct RateLimitDetails {
    primary_window: Option<UsageWindowRaw>,
    secondary_window: Option<UsageWindowRaw>,
}

#[derive(Debug, Deserialize)]
struct AdditionalRateLimitDetails {
    rate_limit: Option<RateLimitDetails>,
}

#[derive(Debug, Deserialize)]
struct UsageWindowRaw {
    used_percent: f64,
    limit_window_seconds: i64,
    reset_at: i64,
}

#[derive(Debug, Deserialize)]
struct CreditDetails {
    has_credits: bool,
    unlimited: bool,
    balance: Option<String>,
}

#[derive(Debug)]
struct ExtractedAuth {
    account_id: String,
    access_token: String,
    email: Option<String>,
    plan_type: Option<String>,
}

impl StoredAccount {
    fn to_summary(&self, current_account_id: Option<&str>) -> AccountSummary {
        AccountSummary {
            id: self.id.clone(),
            label: self.label.clone(),
            email: self.email.clone(),
            account_id: self.account_id.clone(),
            plan_type: self.plan_type.clone(),
            added_at: self.added_at,
            updated_at: self.updated_at,
            usage: self.usage.clone(),
            usage_error: self.usage_error.clone(),
            is_current: current_account_id
                .map(|id| id == self.account_id)
                .unwrap_or(false),
        }
    }
}

fn sync_current_auth_account_on_startup(app: &AppHandle) -> Result<(), String> {
    let auth_json = match read_current_codex_auth_optional()? {
        Some(value) => value,
        None => return Ok(()),
    };

    let extracted = match extract_auth(&auth_json) {
        Ok(value) => value,
        Err(err) => {
            log::warn!("跳过启动自动导入当前账号: {err}");
            return Ok(());
        }
    };

    let mut store = load_store(app)?;
    let already_exists = store
        .accounts
        .iter()
        .any(|account| account.account_id == extracted.account_id);
    if already_exists {
        return Ok(());
    }

    let now = now_unix_seconds();
    let label = extracted
        .email
        .clone()
        .unwrap_or_else(|| format!("Codex {}", short_account(&extracted.account_id)));

    let stored = StoredAccount {
        id: Uuid::new_v4().to_string(),
        label,
        email: extracted.email,
        account_id: extracted.account_id,
        plan_type: extracted.plan_type,
        auth_json,
        added_at: now,
        updated_at: now,
        usage: None,
        usage_error: None,
    };
    store.accounts.push(stored);
    save_store(app, &store)?;
    Ok(())
}

#[tauri::command]
async fn list_accounts(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<Vec<AccountSummary>, String> {
    let _guard = state.store_lock.lock().await;
    let store = load_store(&app)?;
    let current_account_id = current_auth_account_id();
    Ok(store
        .accounts
        .iter()
        .map(|account| account.to_summary(current_account_id.as_deref()))
        .collect())
}

#[tauri::command]
async fn import_current_auth_account(
    app: AppHandle,
    state: State<'_, AppState>,
    label: Option<String>,
) -> Result<AccountSummary, String> {
    let auth_json = read_current_codex_auth()?;
    let extracted = extract_auth(&auth_json)?;

    let usage = fetch_usage_snapshot(&extracted.access_token, &extracted.account_id)
        .await
        .ok();

    let mut _guard = state.store_lock.lock().await;
    let mut store = load_store(&app)?;

    let now = now_unix_seconds();
    let fallback_label = extracted
        .email
        .clone()
        .unwrap_or_else(|| format!("Codex {}", short_account(&extracted.account_id)));
    let new_label = label
        .and_then(|value| {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        })
        .unwrap_or(fallback_label);

    let summary = if let Some(existing) = store
        .accounts
        .iter_mut()
        .find(|account| account.account_id == extracted.account_id)
    {
        existing.label = new_label;
        existing.email = extracted.email;
        existing.plan_type = usage
            .as_ref()
            .and_then(|snapshot| snapshot.plan_type.clone())
            .or(extracted.plan_type)
            .or(existing.plan_type.clone());
        existing.auth_json = auth_json;
        existing.updated_at = now;
        existing.usage = usage;
        existing.usage_error = None;
        existing.to_summary(current_auth_account_id().as_deref())
    } else {
        let stored = StoredAccount {
            id: Uuid::new_v4().to_string(),
            label: new_label,
            email: extracted.email,
            account_id: extracted.account_id,
            plan_type: usage
                .as_ref()
                .and_then(|snapshot| snapshot.plan_type.clone())
                .or(extracted.plan_type),
            auth_json,
            added_at: now,
            updated_at: now,
            usage,
            usage_error: None,
        };
        let summary = stored.to_summary(current_auth_account_id().as_deref());
        store.accounts.push(stored);
        summary
    };

    save_store(&app, &store)?;
    let _ = refresh_macos_tray_snapshot(&app);
    Ok(summary)
}

#[tauri::command]
async fn delete_account(
    app: AppHandle,
    state: State<'_, AppState>,
    id: String,
) -> Result<(), String> {
    let mut _guard = state.store_lock.lock().await;
    let mut store = load_store(&app)?;
    let original_len = store.accounts.len();
    store.accounts.retain(|account| account.id != id);

    if original_len == store.accounts.len() {
        return Err("未找到要删除的账号".to_string());
    }

    save_store(&app, &store)?;
    let _ = refresh_macos_tray_snapshot(&app);
    Ok(())
}

async fn refresh_all_usage_internal(
    app: &AppHandle,
    state: &AppState,
) -> Result<Vec<AccountSummary>, String> {
    let mut store = {
        let _guard = state.store_lock.lock().await;
        load_store(app)?
    };

    for account in &mut store.accounts {
        let fetch_result = match extract_auth(&account.auth_json) {
            Ok(auth) => fetch_usage_snapshot(&auth.access_token, &auth.account_id).await,
            Err(err) => Err(err),
        };

        match fetch_result {
            Ok(snapshot) => {
                account.plan_type = snapshot.plan_type.clone().or(account.plan_type.clone());
                account.updated_at = now_unix_seconds();
                account.usage = Some(snapshot);
                account.usage_error = None;
            }
            Err(err) => {
                account.updated_at = now_unix_seconds();
                account.usage_error = Some(err);
            }
        }
    }

    {
        let _guard = state.store_lock.lock().await;
        save_store(app, &store)?;
    }

    let current_account_id = current_auth_account_id();
    Ok(store
        .accounts
        .iter()
        .map(|account| account.to_summary(current_account_id.as_deref()))
        .collect())
}

#[tauri::command]
async fn refresh_all_usage(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<Vec<AccountSummary>, String> {
    let summaries = refresh_all_usage_internal(&app, state.inner()).await?;
    let _ = update_macos_tray_snapshot(&app, &summaries);
    Ok(summaries)
}

#[tauri::command]
fn detect_codex_app() -> Result<Option<String>, String> {
    Ok(find_codex_app_path().map(|path| path.to_string_lossy().to_string()))
}

#[tauri::command]
fn get_current_auth_status() -> Result<CurrentAuthStatus, String> {
    read_current_auth_status()
}

#[tauri::command]
async fn launch_codex_login(state: State<'_, AppState>) -> Result<(), String> {
    let current_auth = read_current_codex_auth_optional()?;
    {
        let mut backup = state.add_flow_auth_backup.lock().await;
        *backup = Some(current_auth);
    }

    let mut cmd = new_codex_command()?;
    cmd.arg("login")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("无法启动 codex login: {e}"))?;
    Ok(())
}

#[tauri::command]
async fn restore_auth_after_add_flow(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<bool, String> {
    let backup = {
        let mut guard = state.add_flow_auth_backup.lock().await;
        guard.take()
    };

    match backup {
        None => Ok(false),
        Some(Some(auth_json)) => {
            write_active_codex_auth(&auth_json)?;
            let _ = refresh_macos_tray_snapshot(&app);
            Ok(true)
        }
        Some(None) => {
            remove_active_codex_auth()?;
            let _ = refresh_macos_tray_snapshot(&app);
            Ok(true)
        }
    }
}

#[tauri::command]
async fn switch_account_and_launch(
    app: AppHandle,
    state: State<'_, AppState>,
    id: String,
    workspace_path: Option<String>,
) -> Result<SwitchAccountResult, String> {
    let store = {
        let _guard = state.store_lock.lock().await;
        load_store(&app)?
    };

    let account = store
        .accounts
        .iter()
        .find(|account| account.id == id)
        .cloned()
        .ok_or_else(|| "找不到要切换的账号".to_string())?;

    write_active_codex_auth(&account.auth_json)?;
    let _ = refresh_macos_tray_snapshot(&app);

    let app_path = find_codex_app_path();
    if let Some(path) = app_path {
        let _ = Command::new("pkill").args(["-x", "Codex"]).status();
        let mut cmd = Command::new("open");
        cmd.arg("-na").arg(&path);
        if let Some(workspace) = workspace_path.as_deref() {
            cmd.arg(workspace);
        }
        let status = cmd
            .status()
            .map_err(|e| format!("启动 Codex.app 失败: {e}"))?;
        if !status.success() {
            return Err("Codex.app 启动失败".to_string());
        }

        return Ok(SwitchAccountResult {
            account_id: account.account_id,
            launched_app_path: Some(path.to_string_lossy().to_string()),
            used_fallback_cli: false,
        });
    }

    let mut cmd = new_codex_command()?;
    cmd.arg("app");
    if let Some(workspace) = workspace_path.as_deref() {
        cmd.arg(workspace);
    }
    cmd.spawn()
        .map_err(|e| format!("未检测到 Codex.app，且通过 codex app 启动失败: {e}"))?;

    Ok(SwitchAccountResult {
        account_id: account.account_id,
        launched_app_path: None,
        used_fallback_cli: true,
    })
}

fn load_store(app: &AppHandle) -> Result<AccountsStore, String> {
    let path = account_store_path(app)?;
    if !path.exists() {
        return Ok(AccountsStore {
            version: 1,
            accounts: Vec::new(),
        });
    }

    let raw = fs::read_to_string(&path)
        .map_err(|e| format!("读取账号存储文件失败 {}: {e}", path.display()))?;

    serde_json::from_str(&raw).map_err(|e| format!("账号存储文件格式无效 {}: {e}", path.display()))
}

fn save_store(app: &AppHandle, store: &AccountsStore) -> Result<(), String> {
    let path = account_store_path(app)?;
    let parent = path
        .parent()
        .ok_or_else(|| format!("无法解析存储目录 {}", path.display()))?;
    fs::create_dir_all(parent)
        .map_err(|e| format!("创建存储目录失败 {}: {e}", parent.display()))?;

    let serialized =
        serde_json::to_string_pretty(store).map_err(|e| format!("序列化账号存储失败: {e}"))?;
    fs::write(&path, serialized)
        .map_err(|e| format!("写入账号存储文件失败 {}: {e}", path.display()))?;
    set_private_permissions(&path);
    Ok(())
}

fn account_store_path(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("无法获取应用数据目录: {e}"))?;
    Ok(dir.join("accounts.json"))
}

fn read_current_codex_auth() -> Result<Value, String> {
    let path = codex_auth_path()?;
    let raw = fs::read_to_string(&path)
        .map_err(|e| format!("读取当前 Codex 认证文件失败 {}: {e}", path.display()))?;
    serde_json::from_str(&raw).map_err(|e| format!("当前 Codex 认证文件不是合法 JSON: {e}"))
}

fn read_current_codex_auth_optional() -> Result<Option<Value>, String> {
    let path = codex_auth_path()?;
    if !path.exists() {
        return Ok(None);
    }

    let raw = fs::read_to_string(&path)
        .map_err(|e| format!("读取当前 Codex 认证文件失败 {}: {e}", path.display()))?;
    let value =
        serde_json::from_str(&raw).map_err(|e| format!("当前 Codex 认证文件不是合法 JSON: {e}"))?;
    Ok(Some(value))
}

fn read_current_auth_status() -> Result<CurrentAuthStatus, String> {
    let path = codex_auth_path()?;
    if !path.exists() {
        return Ok(CurrentAuthStatus {
            available: false,
            account_id: None,
            email: None,
            plan_type: None,
            auth_mode: None,
            last_refresh: None,
            file_modified_at: None,
            fingerprint: None,
        });
    }

    let metadata = fs::metadata(&path)
        .map_err(|e| format!("读取 auth.json 文件信息失败 {}: {e}", path.display()))?;
    let modified_at = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs() as i64);

    let raw = fs::read_to_string(&path)
        .map_err(|e| format!("读取 auth.json 失败 {}: {e}", path.display()))?;
    let value: Value =
        serde_json::from_str(&raw).map_err(|e| format!("auth.json 不是合法 JSON: {e}"))?;

    let auth_mode = value
        .get("auth_mode")
        .and_then(Value::as_str)
        .map(ToString::to_string);
    let last_refresh = value
        .get("last_refresh")
        .and_then(Value::as_str)
        .map(ToString::to_string);

    let extracted = extract_auth(&value).ok();
    let account_id = extracted.as_ref().map(|auth| auth.account_id.clone());
    let email = extracted.as_ref().and_then(|auth| auth.email.clone());
    let plan_type = extracted.as_ref().and_then(|auth| auth.plan_type.clone());

    let fingerprint = Some(format!(
        "{}|{}|{}|{}",
        account_id.clone().unwrap_or_default(),
        last_refresh.clone().unwrap_or_default(),
        modified_at.unwrap_or_default(),
        auth_mode.clone().unwrap_or_default()
    ));

    Ok(CurrentAuthStatus {
        available: true,
        account_id,
        email,
        plan_type,
        auth_mode,
        last_refresh,
        file_modified_at: modified_at,
        fingerprint,
    })
}

fn write_active_codex_auth(auth_json: &Value) -> Result<(), String> {
    let path = codex_auth_path()?;
    let parent = path
        .parent()
        .ok_or_else(|| format!("无法解析 auth 目录 {}", path.display()))?;
    fs::create_dir_all(parent)
        .map_err(|e| format!("创建 auth 目录失败 {}: {e}", parent.display()))?;

    let serialized = serde_json::to_string_pretty(auth_json)
        .map_err(|e| format!("序列化 auth.json 失败: {e}"))?;
    fs::write(&path, serialized)
        .map_err(|e| format!("写入 auth.json 失败 {}: {e}", path.display()))?;
    set_private_permissions(&path);
    Ok(())
}

fn remove_active_codex_auth() -> Result<(), String> {
    let path = codex_auth_path()?;
    if !path.exists() {
        return Ok(());
    }
    fs::remove_file(&path).map_err(|e| format!("删除 auth.json 失败 {}: {e}", path.display()))
}

fn codex_auth_path() -> Result<PathBuf, String> {
    let home = dirs::home_dir().ok_or_else(|| "无法读取 HOME 目录".to_string())?;
    Ok(home.join(".codex").join("auth.json"))
}

fn extract_auth(auth_json: &Value) -> Result<ExtractedAuth, String> {
    let mode = auth_json
        .get("auth_mode")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_ascii_lowercase();

    let tokens = auth_json
        .get("tokens")
        .and_then(Value::as_object);
    let tokens = match tokens {
        Some(value) => value,
        None => {
            if !mode.is_empty() && mode != "chatgpt" && mode != "chatgpt_auth_tokens" {
                return Err(
                    "当前账号不是 ChatGPT 登录模式，无法读取 Codex 5h/1week 用量。请先执行 codex login。"
                        .to_string(),
                );
            }
            return Err("当前未检测到 ChatGPT 登录令牌，请先执行 codex login。".to_string());
        }
    };

    let access_token = tokens
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or_else(|| "auth.json 缺少 access_token".to_string())?
        .to_string();

    let id_token = tokens
        .get("id_token")
        .and_then(Value::as_str)
        .ok_or_else(|| "auth.json 缺少 id_token".to_string())?;

    let mut account_id = tokens
        .get("account_id")
        .and_then(Value::as_str)
        .map(ToString::to_string);
    let mut email = None;
    let mut plan_type = None;

    if let Ok(claims) = decode_jwt_payload(id_token) {
        email = claims
            .get("email")
            .and_then(Value::as_str)
            .map(ToString::to_string);

        let auth_claim = claims.get("https://api.openai.com/auth");
        if account_id.is_none() {
            account_id = auth_claim
                .and_then(|value| value.get("chatgpt_account_id"))
                .and_then(Value::as_str)
                .map(ToString::to_string);
        }
        plan_type = auth_claim
            .and_then(|value| value.get("chatgpt_plan_type"))
            .and_then(Value::as_str)
            .map(ToString::to_string);
    }

    let account_id =
        account_id.ok_or_else(|| "无法从 auth.json 识别 chatgpt_account_id".to_string())?;

    Ok(ExtractedAuth {
        account_id,
        access_token,
        email,
        plan_type,
    })
}

fn decode_jwt_payload(token: &str) -> Result<Value, String> {
    let payload = token
        .split('.')
        .nth(1)
        .ok_or_else(|| "id_token 格式无效".to_string())?;

    let decoded = URL_SAFE_NO_PAD
        .decode(payload)
        .or_else(|_| {
            let remainder = payload.len() % 4;
            let padded = if remainder == 0 {
                payload.to_string()
            } else {
                format!("{payload}{}", "=".repeat(4 - remainder))
            };
            URL_SAFE.decode(padded)
        })
        .map_err(|e| format!("解码 id_token 失败: {e}"))?;

    serde_json::from_slice(&decoded).map_err(|e| format!("解析 id_token payload 失败: {e}"))
}

fn current_auth_account_id() -> Option<String> {
    read_current_codex_auth().ok().and_then(|auth_json| {
        auth_json
            .get("tokens")
            .and_then(|value| value.get("account_id"))
            .and_then(Value::as_str)
            .map(ToString::to_string)
    })
}

async fn fetch_usage_snapshot(
    access_token: &str,
    account_id: &str,
) -> Result<UsageSnapshot, String> {
    let usage_url = resolve_usage_url();

    let client = reqwest::Client::builder()
        .user_agent("codex-tools/0.1")
        .build()
        .map_err(|e| format!("创建 HTTP 客户端失败: {e}"))?;

    let response = client
        .get(&usage_url)
        .header("Authorization", format!("Bearer {access_token}"))
        .header("ChatGPT-Account-Id", account_id)
        .send()
        .await
        .map_err(|e| format!("请求用量接口失败: {e}"))?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(format!(
            "用量接口返回错误 ({status}): {}",
            truncate_for_error(&body, 240)
        ));
    }

    let payload: UsageApiResponse = response
        .json()
        .await
        .map_err(|e| format!("解析用量接口返回失败: {e}"))?;

    Ok(map_usage_payload(payload))
}

fn resolve_usage_url() -> String {
    let base_url =
        read_chatgpt_base_url_from_config().unwrap_or_else(|| DEFAULT_CHATGPT_BASE_URL.to_string());
    let normalized = base_url.trim_end_matches('/');

    if normalized.contains("/backend-api") {
        format!("{normalized}{WHAM_USAGE_PATH}")
    } else {
        format!("{normalized}{CODEX_USAGE_PATH}")
    }
}

fn read_chatgpt_base_url_from_config() -> Option<String> {
    let home = dirs::home_dir()?;
    let config_path = home.join(".codex").join("config.toml");
    let contents = fs::read_to_string(config_path).ok()?;

    for line in contents.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with("chatgpt_base_url") {
            continue;
        }

        let (_, value) = trimmed.split_once('=')?;
        let cleaned = value.trim().trim_matches('\"').trim_matches('\'');
        if !cleaned.is_empty() {
            return Some(cleaned.to_string());
        }
    }

    None
}

fn map_usage_payload(payload: UsageApiResponse) -> UsageSnapshot {
    let mut windows: Vec<UsageWindowRaw> = Vec::new();

    if let Some(rate_limit) = payload.rate_limit {
        if let Some(primary) = rate_limit.primary_window {
            windows.push(primary);
        }
        if let Some(secondary) = rate_limit.secondary_window {
            windows.push(secondary);
        }
    }

    if let Some(additional) = payload.additional_rate_limits {
        for limit in additional {
            if let Some(rate_limit) = limit.rate_limit {
                if let Some(primary) = rate_limit.primary_window {
                    windows.push(primary);
                }
                if let Some(secondary) = rate_limit.secondary_window {
                    windows.push(secondary);
                }
            }
        }
    }

    let five_hour = pick_nearest_window(&windows, 5 * 60 * 60).map(to_usage_window);
    let one_week = pick_nearest_window(&windows, 7 * 24 * 60 * 60).map(to_usage_window);

    UsageSnapshot {
        fetched_at: now_unix_seconds(),
        plan_type: payload.plan_type,
        five_hour,
        one_week,
        credits: payload.credits.map(|credit| CreditSnapshot {
            has_credits: credit.has_credits,
            unlimited: credit.unlimited,
            balance: credit.balance,
        }),
    }
}

fn pick_nearest_window(windows: &[UsageWindowRaw], target_seconds: i64) -> Option<UsageWindowRaw> {
    windows
        .iter()
        .min_by_key(|window| (window.limit_window_seconds - target_seconds).abs())
        .map(|window| UsageWindowRaw {
            used_percent: window.used_percent,
            limit_window_seconds: window.limit_window_seconds,
            reset_at: window.reset_at,
        })
}

fn to_usage_window(window: UsageWindowRaw) -> UsageWindow {
    UsageWindow {
        used_percent: window.used_percent,
        window_seconds: window.limit_window_seconds,
        reset_at: Some(window.reset_at),
    }
}

fn short_account(account_id: &str) -> String {
    account_id.chars().take(8).collect()
}

fn truncate_for_error(value: &str, max_len: usize) -> String {
    if value.len() <= max_len {
        value.to_string()
    } else {
        format!("{}...", &value[..max_len])
    }
}

fn new_codex_command() -> Result<Command, String> {
    let codex_path = find_codex_cli_path().ok_or_else(|| {
        "未找到 codex 可执行文件。请先安装 Codex CLI，或将其所在目录加入系统 PATH。".to_string()
    })?;

    let mut cmd = Command::new(&codex_path);

    if let Some(parent) = codex_path.parent() {
        let path_entries = if let Some(current_path) = env::var_os("PATH") {
            std::iter::once(parent.to_path_buf())
                .chain(env::split_paths(&current_path))
                .collect::<Vec<_>>()
        } else {
            vec![parent.to_path_buf()]
        };
        let merged = env::join_paths(path_entries).map_err(|e| format!("设置 PATH 失败: {e}"))?;
        cmd.env("PATH", merged);
    }

    Ok(cmd)
}

fn find_codex_cli_path() -> Option<PathBuf> {
    let mut candidates = codex_cli_candidates();
    append_nvm_codex_candidates(&mut candidates);

    let mut seen = HashSet::new();
    for candidate in candidates {
        if !seen.insert(candidate.clone()) {
            continue;
        }
        if is_executable_file(&candidate) {
            return Some(candidate);
        }
    }

    None
}

fn codex_cli_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();

    if let Some(path_os) = env::var_os("PATH") {
        for dir in env::split_paths(&path_os) {
            push_codex_candidates_from_dir(&mut candidates, &dir);
        }
    }

    #[cfg(target_os = "macos")]
    {
        for dir in [
            PathBuf::from("/opt/homebrew/bin"),
            PathBuf::from("/usr/local/bin"),
            PathBuf::from("/usr/bin"),
        ] {
            push_codex_candidates_from_dir(&mut candidates, &dir);
        }
    }

    if let Some(home) = dirs::home_dir() {
        for dir in [
            home.join(".local").join("bin"),
            home.join(".npm-global").join("bin"),
            home.join(".volta").join("bin"),
            home.join(".asdf").join("shims"),
            home.join(".pnpm"),
            home.join("Library").join("pnpm"),
            home.join("bin"),
        ] {
            push_codex_candidates_from_dir(&mut candidates, &dir);
        }
    }

    candidates
}

fn append_nvm_codex_candidates(candidates: &mut Vec<PathBuf>) {
    let Some(home) = dirs::home_dir() else {
        return;
    };
    let nvm_versions_dir = home.join(".nvm").join("versions").join("node");
    let Ok(entries) = fs::read_dir(&nvm_versions_dir) else {
        return;
    };

    let mut version_dirs = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect::<Vec<_>>();
    version_dirs.sort();
    version_dirs.reverse();

    for version_dir in version_dirs {
        push_codex_candidates_from_dir(candidates, &version_dir.join("bin"));
    }
}

fn push_codex_candidates_from_dir(candidates: &mut Vec<PathBuf>, dir: &Path) {
    #[cfg(windows)]
    let names = ["codex.exe", "codex.cmd", "codex.bat"];
    #[cfg(not(windows))]
    let names = ["codex"];

    for name in names {
        candidates.push(dir.join(name));
    }
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn find_codex_app_path() -> Option<PathBuf> {
    let mut candidates = vec![
        PathBuf::from("/Applications/Codex.app"),
        PathBuf::from("/Applications/Codex Desktop.app"),
    ];

    if let Some(home) = dirs::home_dir() {
        candidates.push(home.join("Applications").join("Codex.app"));
        candidates.push(home.join("Applications").join("Codex Desktop.app"));
    }

    if let Some(found) = candidates.into_iter().find(|path| path.exists()) {
        return Some(found);
    }

    let spotlight_queries = [
        "kMDItemFSName == 'Codex.app'",
        "kMDItemCFBundleIdentifier == 'com.openai.codex'",
    ];

    for query in spotlight_queries {
        if let Some(path) = first_spotlight_match(query) {
            return Some(path);
        }
    }

    None
}

fn first_spotlight_match(query: &str) -> Option<PathBuf> {
    let output = Command::new("mdfind").arg(query).output().ok()?;
    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(PathBuf::from)
        .find(|path| path.exists())
}

fn now_unix_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default()
}

fn set_private_permissions(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        if let Ok(metadata) = fs::metadata(path) {
            let mut permissions = metadata.permissions();
            permissions.set_mode(0o600);
            let _ = fs::set_permissions(path, permissions);
        }
    }
}

fn format_percent(value: Option<f64>) -> String {
    value
        .map(|percent| percent.clamp(0.0, 100.0).round() as i64)
        .map(|percent| format!("{percent}%"))
        .unwrap_or_else(|| "--".to_string())
}

fn remaining_percent(window: Option<&UsageWindow>) -> Option<f64> {
    window.map(|item| 100.0 - item.used_percent)
}

#[cfg(target_os = "macos")]
fn tray_account_usage_line(account: &AccountSummary) -> String {
    let five_hour_remaining = format_percent(remaining_percent(
        account
            .usage
            .as_ref()
            .and_then(|usage| usage.five_hour.as_ref()),
    ));
    let one_week_remaining = format_percent(remaining_percent(
        account
            .usage
            .as_ref()
            .and_then(|usage| usage.one_week.as_ref()),
    ));

    let current_prefix = if account.is_current { "[当前] " } else { "" };
    format!(
        "{current_prefix}{} | 5h剩余 {five_hour_remaining} | 1week剩余 {one_week_remaining}",
        account.label
    )
}

#[cfg(target_os = "macos")]
fn build_macos_tray_title(accounts: &[AccountSummary]) -> String {
    if let Some(current) = accounts.iter().find(|account| account.is_current) {
        let five_hour_remaining = format_percent(remaining_percent(
            current
                .usage
                .as_ref()
                .and_then(|usage| usage.five_hour.as_ref()),
        ));
        let one_week_remaining = format_percent(remaining_percent(
            current
                .usage
                .as_ref()
                .and_then(|usage| usage.one_week.as_ref()),
        ));
        return format!("5h {five_hour_remaining} / 1w {one_week_remaining}");
    }

    "5h -- / 1w --".to_string()
}

#[cfg(target_os = "macos")]
fn build_macos_tray_tooltip(accounts: &[AccountSummary]) -> String {
    let mut lines = vec!["Codex Tools 用量".to_string()];

    if let Some(current) = accounts.iter().find(|account| account.is_current) {
        lines.push(format!("当前: {}", tray_account_usage_line(current)));
    } else {
        lines.push("当前: 未检测到正在使用的账号".to_string());
    }

    if accounts.is_empty() {
        lines.push("暂无账号，请先在主窗口添加账号".to_string());
        return lines.join("\n");
    }

    lines.push(format!("全部账号（{}）:", accounts.len()));
    for account in accounts.iter().take(8) {
        lines.push(format!("• {}", tray_account_usage_line(account)));
    }
    if accounts.len() > 8 {
        lines.push(format!("… 还有 {} 个账号", accounts.len() - 8));
    }

    lines.join("\n")
}

#[cfg(target_os = "macos")]
fn build_macos_tray_menu(
    app: &AppHandle,
    accounts: &[AccountSummary],
) -> Result<tauri::menu::Menu<tauri::Wry>, String> {
    use tauri::menu::Menu;
    use tauri::menu::MenuItem;
    use tauri::menu::PredefinedMenuItem;

    let menu = Menu::new(app).map_err(|e| format!("创建状态栏菜单失败: {e}"))?;

    let header = MenuItem::with_id(app, "tray_header", "Codex Tools 用量", false, None::<&str>)
        .map_err(|e| format!("创建状态栏菜单项失败: {e}"))?;
    menu.append(&header)
        .map_err(|e| format!("写入状态栏菜单失败: {e}"))?;

    let current_line = if let Some(current) = accounts.iter().find(|account| account.is_current) {
        format!("当前账号: {}", tray_account_usage_line(current))
    } else {
        "当前账号: 未检测到".to_string()
    };
    let current_item = MenuItem::with_id(
        app,
        "tray_current_summary",
        current_line,
        false,
        None::<&str>,
    )
    .map_err(|e| format!("创建状态栏菜单项失败: {e}"))?;
    menu.append(&current_item)
        .map_err(|e| format!("写入状态栏菜单失败: {e}"))?;

    let separator =
        PredefinedMenuItem::separator(app).map_err(|e| format!("创建状态栏分隔符失败: {e}"))?;
    menu.append(&separator)
        .map_err(|e| format!("写入状态栏菜单失败: {e}"))?;

    if accounts.is_empty() {
        let empty = MenuItem::with_id(
            app,
            "tray_accounts_empty",
            "暂无账号（请在主窗口添加）",
            false,
            None::<&str>,
        )
        .map_err(|e| format!("创建状态栏菜单项失败: {e}"))?;
        menu.append(&empty)
            .map_err(|e| format!("写入状态栏菜单失败: {e}"))?;
    } else {
        for (index, account) in accounts.iter().enumerate() {
            let id = format!("tray_account_{index}");
            let line_item = MenuItem::with_id(
                app,
                id,
                tray_account_usage_line(account),
                false,
                None::<&str>,
            )
            .map_err(|e| format!("创建状态栏菜单项失败: {e}"))?;
            menu.append(&line_item)
                .map_err(|e| format!("写入状态栏菜单失败: {e}"))?;
        }
    }

    let separator =
        PredefinedMenuItem::separator(app).map_err(|e| format!("创建状态栏分隔符失败: {e}"))?;
    menu.append(&separator)
        .map_err(|e| format!("写入状态栏菜单失败: {e}"))?;

    let refresh = MenuItem::with_id(
        app,
        TRAY_MENU_REFRESH_ID,
        "立即刷新用量",
        true,
        None::<&str>,
    )
    .map_err(|e| format!("创建状态栏菜单项失败: {e}"))?;
    let open = MenuItem::with_id(
        app,
        TRAY_MENU_OPEN_ID,
        "打开 Codex Tools",
        true,
        None::<&str>,
    )
    .map_err(|e| format!("创建状态栏菜单项失败: {e}"))?;
    let quit = MenuItem::with_id(app, TRAY_MENU_QUIT_ID, "退出", true, None::<&str>)
        .map_err(|e| format!("创建状态栏菜单项失败: {e}"))?;

    menu.append(&refresh)
        .map_err(|e| format!("写入状态栏菜单失败: {e}"))?;
    menu.append(&open)
        .map_err(|e| format!("写入状态栏菜单失败: {e}"))?;
    menu.append(&quit)
        .map_err(|e| format!("写入状态栏菜单失败: {e}"))?;

    Ok(menu)
}

#[cfg(target_os = "macos")]
fn update_macos_tray_snapshot(app: &AppHandle, accounts: &[AccountSummary]) -> Result<(), String> {
    let tray = app
        .tray_by_id(TRAY_ID)
        .ok_or_else(|| "状态栏尚未初始化".to_string())?;

    let menu = build_macos_tray_menu(app, accounts)?;
    tray.set_menu(Some(menu))
        .map_err(|e| format!("更新状态栏菜单失败: {e}"))?;
    tray.set_title(Some(build_macos_tray_title(accounts)))
        .map_err(|e| format!("更新状态栏标题失败: {e}"))?;
    tray.set_tooltip(Some(build_macos_tray_tooltip(accounts)))
        .map_err(|e| format!("更新状态栏提示失败: {e}"))?;
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn update_macos_tray_snapshot(
    _app: &AppHandle,
    _accounts: &[AccountSummary],
) -> Result<(), String> {
    Ok(())
}

#[cfg(target_os = "macos")]
fn refresh_macos_tray_snapshot(app: &AppHandle) -> Result<(), String> {
    let store = load_store(app)?;
    let current_account_id = current_auth_account_id();
    let summaries: Vec<AccountSummary> = store
        .accounts
        .iter()
        .map(|account| account.to_summary(current_account_id.as_deref()))
        .collect();
    update_macos_tray_snapshot(app, &summaries)
}

#[cfg(not(target_os = "macos"))]
fn refresh_macos_tray_snapshot(_app: &AppHandle) -> Result<(), String> {
    Ok(())
}

#[cfg(target_os = "macos")]
fn start_macos_tray_refresh_loop(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        loop {
            let state = app.state::<AppState>();
            if let Ok(summaries) = refresh_all_usage_internal(&app, state.inner()).await {
                let _ = update_macos_tray_snapshot(&app, &summaries);
            }
            tokio::time::sleep(Duration::from_secs(REFRESH_INTERVAL_SECONDS)).await;
        }
    });
}

#[cfg(target_os = "macos")]
fn setup_macos_status_bar(app: &AppHandle) -> Result<(), String> {
    use tauri::tray::TrayIconBuilder;

    let store = load_store(app)?;
    let current_account_id = current_auth_account_id();
    let summaries: Vec<AccountSummary> = store
        .accounts
        .iter()
        .map(|account| account.to_summary(current_account_id.as_deref()))
        .collect();
    let menu = build_macos_tray_menu(app, &summaries)?;

    TrayIconBuilder::with_id(TRAY_ID)
        .menu(&menu)
        .icon(STATUS_BAR_ICON)
        .icon_as_template(true)
        .title(build_macos_tray_title(&summaries))
        .tooltip(build_macos_tray_tooltip(&summaries))
        .show_menu_on_left_click(true)
        .build(app)
        .map_err(|e| format!("创建 macOS 状态栏失败: {e}"))?;

    start_macos_tray_refresh_loop(app.clone());
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn setup_macos_status_bar(_app: &AppHandle) -> Result<(), String> {
    Ok(())
}

fn handle_status_bar_menu_event(app: &AppHandle, event: tauri::menu::MenuEvent) {
    #[cfg(target_os = "macos")]
    {
        let id = event.id().as_ref();
        if id == TRAY_MENU_QUIT_ID {
            app.exit(0);
            return;
        }

        if id == TRAY_MENU_OPEN_ID {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.unminimize();
                let _ = window.show();
                let _ = window.set_focus();
            }
            return;
        }

        if id == TRAY_MENU_REFRESH_ID {
            let app_handle = app.clone();
            tauri::async_runtime::spawn(async move {
                let state = app_handle.state::<AppState>();
                if let Ok(summaries) = refresh_all_usage_internal(&app_handle, state.inner()).await
                {
                    let _ = update_macos_tray_snapshot(&app_handle, &summaries);
                }
            });
        }
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .on_menu_event(handle_status_bar_menu_event)
        .setup(|app| {
            if cfg!(debug_assertions) {
                app.handle().plugin(
                    tauri_plugin_log::Builder::default()
                        .level(log::LevelFilter::Info)
                        .build(),
                )?;
            }
            sync_current_auth_account_on_startup(app.handle())?;
            setup_macos_status_bar(app.handle())?;
            Ok(())
        })
        .manage(AppState::default())
        .invoke_handler(tauri::generate_handler![
            list_accounts,
            import_current_auth_account,
            delete_account,
            refresh_all_usage,
            detect_codex_app,
            get_current_auth_status,
            launch_codex_login,
            restore_auth_after_add_flow,
            switch_account_and_launch
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
