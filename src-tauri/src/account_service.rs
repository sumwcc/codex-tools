use std::collections::HashMap;

use tauri::AppHandle;

use crate::auth::current_auth_account_id;
use crate::auth::extract_auth;
use crate::auth::read_current_codex_auth;
use crate::auth::read_current_codex_auth_optional;
use crate::auth::refresh_chatgpt_auth_tokens;
use crate::models::AccountSummary;
use crate::models::StoredAccount;
use crate::state::AppState;
use crate::store::load_store;
use crate::store::save_store;
use crate::usage::fetch_usage_snapshot;
use crate::utils::now_unix_seconds;
use crate::utils::short_account;

const DEACTIVATED_WORKSPACE_NOTICE: &str =
    "该账号已被踢出 team 组织，请重新授权后再刷新。";

pub(crate) async fn list_accounts_internal(
    app: &AppHandle,
    state: &AppState,
) -> Result<Vec<AccountSummary>, String> {
    let _guard = state.store_lock.lock().await;
    let store = load_store(app)?;
    let current_account_id = current_auth_account_id();
    Ok(store
        .accounts
        .iter()
        .map(|account| account.to_summary(current_account_id.as_deref()))
        .collect())
}

pub(crate) async fn import_current_auth_account_internal(
    app: &AppHandle,
    state: &AppState,
    label: Option<String>,
) -> Result<AccountSummary, String> {
    let auth_json = read_current_codex_auth()?;
    let extracted = extract_auth(&auth_json)?;

    // 用量拉取失败不阻断导入流程，避免账号无法入库。
    let usage = fetch_usage_snapshot(&extracted.access_token, &extracted.account_id)
        .await
        .ok();

    let mut _guard = state.store_lock.lock().await;
    let mut store = load_store(app)?;

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
            id: uuid::Uuid::new_v4().to_string(),
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

    save_store(app, &store)?;
    Ok(summary)
}

pub(crate) async fn delete_account_internal(
    app: &AppHandle,
    state: &AppState,
    id: &str,
) -> Result<(), String> {
    let mut _guard = state.store_lock.lock().await;
    let mut store = load_store(app)?;
    let original_len = store.accounts.len();
    store.accounts.retain(|account| account.id != id);

    if original_len == store.accounts.len() {
        return Err("未找到要删除的账号".to_string());
    }

    save_store(app, &store)?;
    Ok(())
}

/// 拉取并刷新所有账号用量，返回可直接用于前端/状态栏显示的摘要。
///
/// 为避免“后台刷新覆盖新增账号”的竞态：
/// 1) 先拿快照用于网络请求；
/// 2) 请求完成后重新加载最新 store 并按 account_id 合并写回。
pub(crate) async fn refresh_all_usage_internal(
    app: &AppHandle,
    state: &AppState,
    force_auth_refresh: bool,
) -> Result<Vec<AccountSummary>, String> {
    #[derive(Debug)]
    struct RefreshTarget {
        account_id: String,
        auth_json: serde_json::Value,
        auth_is_current: bool,
    }

    let current_auth_override: Option<(String, serde_json::Value)> =
        read_current_codex_auth_optional()
            .ok()
            .flatten()
            .and_then(|auth_json| {
                extract_auth(&auth_json)
                    .ok()
                    .map(|extracted| (extracted.account_id, auth_json))
            });

    let refresh_targets: Vec<RefreshTarget> = {
        let _guard = state.store_lock.lock().await;
        let store = load_store(app)?;
        store
            .accounts
            .into_iter()
            .map(|account| {
                let (auth_json, auth_is_current) = current_auth_override
                    .as_ref()
                    .and_then(|(account_id, auth_json)| {
                        if account_id == &account.account_id {
                            Some((auth_json.clone(), true))
                        } else {
                            None
                        }
                    })
                    .unwrap_or((account.auth_json, false));

                RefreshTarget {
                    account_id: account.account_id,
                    auth_json,
                    auth_is_current,
                }
            })
            .collect()
    };

    #[derive(Debug)]
    struct RefreshOutcome {
        usage: Option<crate::models::UsageSnapshot>,
        usage_error: Option<String>,
        updated_at: i64,
        auth_plan_type: Option<String>,
        auth_email: Option<String>,
        auth_json: serde_json::Value,
        auth_is_current: bool,
        auth_refreshed: bool,
    }

    let mut outcomes: HashMap<String, RefreshOutcome> = HashMap::new();
    let mut handles = Vec::with_capacity(refresh_targets.len());
    for target in refresh_targets {
        handles.push(tauri::async_runtime::spawn(async move {
            let mut working_auth_json = target.auth_json;
            let mut refresh_error: Option<String> = None;
            let mut auth_refreshed = false;

            if force_auth_refresh {
                match refresh_chatgpt_auth_tokens(&working_auth_json).await {
                    Ok(refreshed) => {
                        working_auth_json = refreshed;
                        auth_refreshed = true;
                    }
                    Err(err) => {
                        refresh_error = Some(err);
                    }
                }
            }

            let mut extracted = extract_auth(&working_auth_json);
            let mut fetch_result = match &extracted {
                Ok(auth) => fetch_usage_snapshot(&auth.access_token, &auth.account_id).await,
                Err(err) => Err(err.clone()),
            };

            if !force_auth_refresh && should_retry_with_token_refresh(&fetch_result) {
                match refresh_chatgpt_auth_tokens(&working_auth_json).await {
                    Ok(refreshed) => {
                        working_auth_json = refreshed;
                        auth_refreshed = true;
                        extracted = extract_auth(&working_auth_json);
                        fetch_result = match &extracted {
                            Ok(auth) => {
                                fetch_usage_snapshot(&auth.access_token, &auth.account_id).await
                            }
                            Err(err) => Err(err.clone()),
                        };
                    }
                    Err(err) => {
                        refresh_error = Some(err);
                    }
                }
            }

            let (auth_plan_type, auth_email) = match &extracted {
                Ok(auth) => (auth.plan_type.clone(), auth.email.clone()),
                Err(_) => (None, None),
            };

            let updated_at = now_unix_seconds();
            let outcome = match fetch_result {
                Ok(snapshot) => RefreshOutcome {
                    usage: Some(snapshot),
                    usage_error: None,
                    updated_at,
                    auth_plan_type,
                    auth_email,
                    auth_json: working_auth_json,
                    auth_is_current: target.auth_is_current,
                    auth_refreshed,
                },
                Err(err) => {
                    let combined_error = if let Some(refresh_err) = refresh_error {
                        format!("{err} | 令牌刷新失败: {refresh_err}")
                    } else {
                        err
                    };
                    let display_error = normalize_usage_error_message(&combined_error);
                    RefreshOutcome {
                        usage: None,
                        usage_error: Some(display_error),
                        updated_at,
                        auth_plan_type,
                        auth_email,
                        auth_json: working_auth_json,
                        auth_is_current: target.auth_is_current,
                        auth_refreshed,
                    }
                }
            };
            (target.account_id, outcome)
        }));
    }

    for handle in handles {
        match handle.await {
            Ok((account_id, outcome)) => {
                outcomes.insert(account_id, outcome);
            }
            Err(err) => {
                log::warn!("并行刷新账号用量任务异常: {err}");
            }
        }
    }

    let store = {
        let _guard = state.store_lock.lock().await;
        let mut latest_store = load_store(app)?;

        for account in &mut latest_store.accounts {
            let Some(outcome) = outcomes.get(&account.account_id) else {
                continue;
            };

            account.updated_at = outcome.updated_at;
            account.auth_json = outcome.auth_json.clone();
            account.email = outcome.auth_email.clone().or(account.email.clone());
            let trusted_auth_plan_type = if outcome.auth_is_current || outcome.auth_refreshed {
                outcome.auth_plan_type.clone()
            } else {
                None
            };
            if let Some(snapshot) = outcome.usage.clone() {
                let mut resolved_snapshot = snapshot;
                let resolved_plan_type = trusted_auth_plan_type
                    .clone()
                    .or(resolved_snapshot.plan_type.clone())
                    .or(account.plan_type.clone());
                resolved_snapshot.plan_type = resolved_plan_type.clone();
                account.plan_type = resolved_plan_type;
                account.usage = Some(resolved_snapshot);
                account.usage_error = None;
            } else if let Some(err) = outcome.usage_error.clone() {
                if trusted_auth_plan_type.is_some() {
                    account.plan_type = trusted_auth_plan_type;
                }
                account.usage_error = Some(err);
            }
        }

        save_store(app, &latest_store)?;
        latest_store
    };

    // 与当前 auth 文件重新对齐，确保 current 标签准确。
    let current_account_id = current_auth_account_id();
    let summaries: Vec<AccountSummary> = store
        .accounts
        .iter()
        .map(|account| account.to_summary(current_account_id.as_deref()))
        .collect();

    Ok(summaries)
}

fn should_retry_with_token_refresh(
    fetch_result: &Result<crate::models::UsageSnapshot, String>,
) -> bool {
    match fetch_result {
        Ok(snapshot) => snapshot.plan_type.is_none(),
        Err(err) => {
            let normalized = err.to_ascii_lowercase();
            normalized.contains("401")
                || normalized.contains("unauthorized")
                || normalized.contains("invalid_token")
                || normalized.contains("deactivated_workspace")
        }
    }
}

fn normalize_usage_error_message(raw_error: &str) -> String {
    let normalized = raw_error.to_ascii_lowercase();
    if normalized.contains("deactivated_workspace") {
        return DEACTIVATED_WORKSPACE_NOTICE.to_string();
    }
    raw_error.to_string()
}
