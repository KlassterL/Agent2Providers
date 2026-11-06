use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use tokio::sync::mpsc;

use agent2api_provider_protocol as proto;
use serde_json::Value;

use super::{
    accounts, callback_server, endpoints,
    oauth::{self, ProxyChoice},
};

const FLOW_PREFIX: &str = "codex-web-";
const PENDING_TTL_MS: i64 = 10 * 60 * 1000;

struct PendingLogin {
    verifier: String,
    redirect_uri: String,
    proxy: ProxyChoice,
    created_at: i64,
    sender: mpsc::UnboundedSender<String>,
    receiver: mpsc::UnboundedReceiver<String>,
}

fn flows() -> &'static Mutex<HashMap<String, PendingLogin>> {
    static FLOWS: OnceLock<Mutex<HashMap<String, PendingLogin>>> = OnceLock::new();
    FLOWS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn lock_flows() -> std::sync::MutexGuard<'static, HashMap<String, PendingLogin>> {
    match flows().lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn sweep(table: &mut HashMap<String, PendingLogin>) {
    let now = crate::logging::now_ms();
    table.retain(|_, flow| now.saturating_sub(flow.created_at) <= PENDING_TTL_MS);
}

pub async fn login_start(method: &str, payload: &Value) -> Result<proto::LoginStart, String> {
    if method != "browser" {
        return Err("Codex 账号请使用网页登录添加".to_string());
    }
    let callback_port = callback_server::ensure_running().await?;
    let redirect_uri = endpoints::redirect_uri(callback_port);
    let verifier = oauth::random_secret().ok_or_else(|| "无法生成登录凭证".to_string())?;
    let state = oauth::random_secret().ok_or_else(|| "无法生成登录状态".to_string())?;
    let url =
        oauth::authorize_url(&verifier, &state, &redirect_uri).map_err(|error| error.message)?;
    let proxy = oauth::proxy_for_login(payload.get("proxy")).map_err(|error| error.message)?;
    let (sender, receiver) = mpsc::unbounded_channel();
    let flow = format!("{FLOW_PREFIX}{state}");
    let mut table = lock_flows();
    sweep(&mut table);
    table.insert(
        state.clone(),
        PendingLogin {
            verifier,
            redirect_uri,
            proxy,
            created_at: crate::logging::now_ms(),
            sender,
            receiver,
        },
    );
    Ok(proto::LoginStart {
        flow,
        url,
        instructions: "登录完成后会自动加入账号列表。".to_string(),
        poll_interval_ms: 1200,
        external_browser: payload.get("mode").and_then(Value::as_str) == Some("external"),
        allowed_hosts: vec!["auth.openai.com".to_string(), "chatgpt.com".to_string()],
    })
}

pub fn accept_callback(params: &HashMap<String, String>) -> bool {
    let state = params.get("state").map(String::as_str).unwrap_or("").trim();
    let has_code = params
        .get("code")
        .is_some_and(|code| !code.trim().is_empty());
    let has_error = params
        .get("error")
        .is_some_and(|error| !error.trim().is_empty());
    if state.is_empty() || (!has_code && !has_error) {
        return false;
    }
    let mut table = lock_flows();
    sweep(&mut table);
    let Some(flow) = table.get(state) else {
        return false;
    };
    let serialized = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(params.iter())
        .finish();
    flow.sender.send(serialized).is_ok()
}

pub async fn login_poll(flow_id: &str) -> Result<proto::LoginPoll, String> {
    let Some(state) = flow_id.strip_prefix(FLOW_PREFIX) else {
        return Err("Codex 登录流程不存在或已过期，请重新开始".to_string());
    };
    enum PollState {
        Pending,
        Query(String),
        Closed,
    }
    let (poll, pending) = {
        let mut table = lock_flows();
        sweep(&mut table);
        let state_result = match table.get_mut(state) {
            Some(flow) => match flow.receiver.try_recv() {
                Ok(query) => PollState::Query(query),
                Err(mpsc::error::TryRecvError::Empty) => PollState::Pending,
                Err(mpsc::error::TryRecvError::Disconnected) => PollState::Closed,
            },
            None => return Err("Codex 登录流程已过期，请重新开始".to_string()),
        };
        match state_result {
            PollState::Pending => (Some(proto::LoginPoll::Pending { hint: None }), None),
            PollState::Closed => {
                table.remove(state);
                (
                    Some(proto::LoginPoll::Failed {
                        message: "Codex 回调监听已关闭，请重新开始登录".to_string(),
                    }),
                    None,
                )
            }
            PollState::Query(query) => (None, table.remove(state).map(|flow| (flow, query))),
        }
    };
    if let Some(poll) = poll {
        return Ok(poll);
    }
    let Some((pending, query)) = pending else {
        return Err("Codex 登录流程已过期，请重新开始".to_string());
    };
    complete_query(state, &query, pending).await
}

pub async fn login_callback(callback: proto::LoginCallback) -> Result<proto::LoginPoll, String> {
    let Some(state) = callback.flow.strip_prefix(FLOW_PREFIX) else {
        return Err("Codex 登录流程不存在或已过期，请重新开始".to_string());
    };
    let source = if callback.url.trim().is_empty() {
        callback.code.as_str()
    } else {
        callback.url.as_str()
    };
    let query =
        query_from_callback(source, state).ok_or_else(|| "Codex 回调地址无效".to_string())?;
    let pending = {
        let mut table = lock_flows();
        sweep(&mut table);
        table.remove(state)
    }
    .ok_or_else(|| "Codex 登录流程已过期，请重新开始".to_string())?;
    complete_query(state, &query, pending).await
}

pub async fn login_cancel(flow_id: &str) -> Result<(), String> {
    if let Some(state) = flow_id.strip_prefix(FLOW_PREFIX) {
        lock_flows().remove(state);
    }
    Ok(())
}

fn query_from_callback(source: &str, state: &str) -> Option<String> {
    if let Ok(url) = url::Url::parse(source.trim()) {
        if url.path() != endpoints::CALLBACK_PATH {
            return None;
        }
        let pairs = url.query_pairs().into_owned().collect::<Vec<_>>();
        return Some(
            url::form_urlencoded::Serializer::new(String::new())
                .extend_pairs(pairs)
                .finish(),
        );
    }
    let text = source.trim().trim_start_matches('?');
    if text.contains('=') {
        return Some(text.to_string());
    }
    (!text.is_empty()).then(|| {
        url::form_urlencoded::Serializer::new(String::new())
            .append_pair("state", state)
            .append_pair("code", text)
            .finish()
    })
}

async fn complete_query(
    state: &str,
    query: &str,
    pending: PendingLogin,
) -> Result<proto::LoginPoll, String> {
    let params = url::form_urlencoded::parse(query.as_bytes())
        .into_owned()
        .collect::<HashMap<String, String>>();
    let callback_state = params.get("state").map(String::as_str).unwrap_or("");
    if callback_state != state {
        return Err("Codex 回调与当前登录流程不匹配".to_string());
    }
    if let Some(error) = params.get("error").filter(|value| !value.trim().is_empty()) {
        let detail = params
            .get("error_description")
            .map(String::as_str)
            .unwrap_or("");
        return Err(if detail.is_empty() {
            format!("登录失败：{error}")
        } else {
            format!("登录失败：{error}（{detail}）")
        });
    }
    let code = params.get("code").map(String::as_str).unwrap_or("").trim();
    if code.is_empty() {
        return Err("Codex 回调缺少授权码，请重新开始登录".to_string());
    }
    let credentials = oauth::exchange_code(
        code,
        &pending.verifier,
        &pending.redirect_uri,
        &pending.proxy,
    )
    .await
    .map_err(|error| error.message)?;
    Ok(proto::LoginPoll::Done {
        account: accounts::account_of(&credentials, "web"),
    })
}
