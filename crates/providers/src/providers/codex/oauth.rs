use std::sync::OnceLock;
use std::time::Duration;

use agent2api_provider_protocol::WireProxy;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::errors::GatewayError;
use crate::flight::{Join, Table};

use super::{credentials::CodexCredentials, endpoints, PROVIDER_ID};

const DEFAULT_EXPIRES_IN_SECONDS: i64 = 3600;
const MAX_ERROR_CHARS: usize = 320;

#[derive(Clone, Debug)]
pub enum ProxyChoice {
    System,
    Direct,
    Explicit(WireProxy),
}

fn flights() -> &'static Table<CodexCredentials> {
    static FLIGHTS: OnceLock<Table<CodexCredentials>> = OnceLock::new();
    FLIGHTS.get_or_init(Table::new)
}

pub fn random_secret() -> Option<String> {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).ok()?;
    Some(URL_SAFE_NO_PAD.encode(bytes))
}

pub fn authorize_url(
    verifier: &str,
    state: &str,
    redirect_uri: &str,
) -> Result<String, GatewayError> {
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let mut url = url::Url::parse(endpoints::AUTH_URL)
        .map_err(|_| GatewayError::with_status(500, "登录地址无效"))?;
    url.query_pairs_mut()
        .append_pair("client_id", endpoints::CLIENT_ID)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("response_type", "code")
        .append_pair("scope", endpoints::SCOPES)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state)
        .append_pair("prompt", "login")
        .append_pair("id_token_add_organizations", "true")
        .append_pair("codex_cli_simplified_flow", "true");
    Ok(url.to_string())
}

pub fn proxy_for_login(value: Option<&Value>) -> Result<ProxyChoice, GatewayError> {
    proxy_choice(value)
}

fn proxy_for_account(record: &Value) -> Result<ProxyChoice, GatewayError> {
    let original = record.get("_agent2apiOriginalProxy");
    let current = record.get("proxy");
    let selected = if current.is_some_and(Value::is_null) {
        original.or(current)
    } else {
        current.or(original)
    };
    proxy_choice(selected)
}

fn proxy_choice(value: Option<&Value>) -> Result<ProxyChoice, GatewayError> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok(ProxyChoice::System);
    };
    let candidate = value
        .get("proxy")
        .filter(|proxy| proxy.is_object())
        .unwrap_or(value);
    if candidate
        .get("source")
        .and_then(Value::as_str)
        .is_some_and(|source| source.eq_ignore_ascii_case("direct"))
    {
        return Ok(ProxyChoice::Direct);
    }
    if let Some(message) = candidate.get("error").and_then(Value::as_str) {
        return Err(GatewayError::with_status(400, message.to_string()));
    }
    match crate::proxy_util::resolve_account_proxy(Some(candidate)) {
        Some(crate::proxy_util::ProxyResolution::Resolved(proxy)) => {
            Ok(ProxyChoice::Explicit(proxy))
        }
        Some(crate::proxy_util::ProxyResolution::Failed(message)) => {
            Err(GatewayError::with_status(400, message))
        }
        None => Ok(ProxyChoice::System),
    }
}

fn client_for(proxy: &ProxyChoice) -> reqwest::Client {
    match proxy {
        ProxyChoice::System => crate::http::control_client_system_proxy(),
        ProxyChoice::Direct => crate::http::control_client(None, "direct"),
        ProxyChoice::Explicit(proxy) => crate::http::control_client(Some(proxy), ""),
    }
}

fn truncate(text: &str) -> String {
    let cleaned: String = text
        .chars()
        .filter(|character| !character.is_control())
        .collect();
    match cleaned.char_indices().nth(MAX_ERROR_CHARS) {
        Some((index, _)) => format!("{}…", &cleaned[..index]),
        None => cleaned,
    }
}

async fn post_token_form(
    fields: &[(&str, &str)],
    proxy: &ProxyChoice,
) -> Result<(u16, String), GatewayError> {
    let response = client_for(proxy)
        .post(endpoints::TOKEN_URL)
        .header("Accept", "application/json")
        .header("User-Agent", endpoints::USER_AGENT)
        .timeout(Duration::from_millis(endpoints::REQUEST_TIMEOUT_MS))
        .form(fields)
        .send()
        .await
        .map_err(|error| {
            GatewayError::with_status(
                502,
                format!("登录请求失败：{}", crate::http::describe_chain(&error)),
            )
        })?;
    let status = response.status().as_u16();
    let text = response.text().await.unwrap_or_default();
    Ok((status, text))
}

fn parse_token_payload(status: u16, text: &str, context: &str) -> Result<Value, GatewayError> {
    let payload: Value = serde_json::from_str(text).unwrap_or(Value::Null);
    if !(200..300).contains(&status) {
        let code = payload
            .get("error")
            .and_then(|value| {
                value
                    .as_str()
                    .or_else(|| value.get("code").and_then(Value::as_str))
            })
            .unwrap_or("")
            .trim();
        let detail = payload
            .pointer("/error/message")
            .or_else(|| payload.get("error_description"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| truncate(text));
        let error_status = if code == "invalid_grant" {
            401
        } else if status == 429 {
            429
        } else {
            502
        };
        let hint = if detail.is_empty() {
            String::new()
        } else {
            format!("：{detail}")
        };
        return Err(GatewayError::with_status(
            error_status,
            format!("Codex {context}失败（HTTP {status}，{code}）{hint}"),
        ));
    }
    Ok(payload)
}

fn token_string(payload: &Value, key: &str) -> String {
    payload
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("")
        .to_string()
}

fn expires_in(payload: &Value) -> i64 {
    match payload.get("expires_in") {
        Some(Value::Number(number)) => number.as_i64().unwrap_or(DEFAULT_EXPIRES_IN_SECONDS),
        Some(Value::String(text)) => text.trim().parse().unwrap_or(DEFAULT_EXPIRES_IN_SECONDS),
        _ => DEFAULT_EXPIRES_IN_SECONDS,
    }
}

fn jwt_payload(token: &str) -> Option<Value> {
    let encoded = token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(encoded).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn auth_claim(payload: &Value, name: &str) -> String {
    payload
        .get("https://api.openai.com/auth")
        .and_then(|value| value.get(name))
        .and_then(Value::as_str)
        .or_else(|| {
            payload
                .pointer(&format!("/https:~1~1api.openai.com~1auth/{name}"))
                .and_then(Value::as_str)
        })
        .unwrap_or("")
        .trim()
        .to_string()
}

fn or_else_empty(value: String, next: impl FnOnce() -> String) -> String {
    if value.is_empty() {
        next()
    } else {
        value
    }
}

fn credentials_from_tokens(
    payload: &Value,
    previous: Option<&CodexCredentials>,
) -> Result<CodexCredentials, GatewayError> {
    let access_token = token_string(payload, "access_token");
    let refresh_token = token_string(payload, "refresh_token");
    if access_token.is_empty() {
        return Err(GatewayError::with_status(502, "登录响应缺少 access_token"));
    }
    let id_token = token_string(payload, "id_token");
    let id_claims = jwt_payload(&id_token).unwrap_or(Value::Null);
    let access_claims = jwt_payload(&access_token).unwrap_or(Value::Null);
    let account_id = or_else_empty(auth_claim(&id_claims, "chatgpt_account_id"), || {
        auth_claim(&access_claims, "chatgpt_account_id")
    });
    let user_id = or_else_empty(auth_claim(&id_claims, "user_id"), || {
        or_else_empty(auth_claim(&access_claims, "user_id"), || {
            id_claims
                .get("sub")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        })
    });
    let email = id_claims
        .get("email")
        .and_then(Value::as_str)
        .or_else(|| access_claims.get("email").and_then(Value::as_str))
        .or_else(|| previous.map(|credentials| credentials.email.as_str()))
        .unwrap_or("")
        .to_string();
    let plan_type = or_else_empty(auth_claim(&id_claims, "chatgpt_plan_type"), || {
        auth_claim(&access_claims, "chatgpt_plan_type")
    });
    let account_id = if account_id.is_empty() {
        previous
            .map(|credentials| credentials.account_id.clone())
            .unwrap_or_default()
    } else {
        account_id
    };
    if account_id.is_empty() {
        return Err(GatewayError::with_status(502, "登录凭证缺少账号标识"));
    }
    let expires = expires_in(payload).clamp(60, 7 * 24 * 3600);
    Ok(CodexCredentials {
        id: previous
            .map(|credentials| credentials.id.clone())
            .unwrap_or_default(),
        access_token,
        refresh_token: if refresh_token.is_empty() {
            previous
                .map(|credentials| credentials.refresh_token.clone())
                .unwrap_or_default()
        } else {
            refresh_token
        },
        expires_at: crate::logging::now_ms() + expires * 1000,
        account_id,
        user_id: if user_id.is_empty() {
            previous.map(|c| c.user_id.clone()).unwrap_or_default()
        } else {
            user_id
        },
        email,
        plan_type: if plan_type.is_empty() {
            previous.map(|c| c.plan_type.clone()).unwrap_or_default()
        } else {
            plan_type
        },
    })
}

pub async fn exchange_code(
    code: &str,
    verifier: &str,
    redirect_uri: &str,
    proxy: &ProxyChoice,
) -> Result<CodexCredentials, GatewayError> {
    let (status, text) = post_token_form(
        &[
            ("grant_type", "authorization_code"),
            ("client_id", endpoints::CLIENT_ID),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("code_verifier", verifier),
        ],
        proxy,
    )
    .await?;
    let payload = parse_token_payload(status, &text, "授权码兑换")?;
    let credentials = credentials_from_tokens(&payload, None)?;
    if credentials.refresh_token.is_empty() {
        return Err(GatewayError::with_status(
            502,
            "登录响应缺少 refresh_token，请撤销授权后重新登录",
        ));
    }
    Ok(credentials)
}

async fn refresh_access_token(
    refresh_token: &str,
    proxy: &ProxyChoice,
) -> Result<(String, Option<String>, i64), GatewayError> {
    let refresh_token = refresh_token.trim();
    if refresh_token.is_empty() {
        return Err(GatewayError::with_status(
            401,
            "Codex 账号缺少 refresh token，请重新登录",
        ));
    }
    let (status, text) = post_token_form(
        &[
            ("grant_type", "refresh_token"),
            ("client_id", endpoints::CLIENT_ID),
            ("refresh_token", refresh_token),
        ],
        proxy,
    )
    .await?;
    let payload = parse_token_payload(status, &text, "token 刷新")?;
    let access_token = token_string(&payload, "access_token");
    if access_token.is_empty() {
        return Err(GatewayError::with_status(
            502,
            "Codex 刷新响应缺少 access_token",
        ));
    }
    let refresh = token_string(&payload, "refresh_token");
    let expiry = expires_in(&payload).clamp(60, 7 * 24 * 3600);
    Ok((
        access_token,
        (!refresh.is_empty()).then_some(refresh),
        expiry,
    ))
}

pub async fn ensure_fresh(
    ctx: &agent2api_provider_sdk::PluginCtx,
    account: &str,
    force: bool,
) -> Result<CodexCredentials, GatewayError> {
    let record = crate::store::record(ctx, PROVIDER_ID, account).await?;
    let credentials = super::credentials::from_record(&record);
    if !force && !credentials.expiring() {
        return Ok(credentials);
    }
    if credentials.refresh_token.trim().is_empty() {
        return Err(GatewayError::with_status(
            401,
            "Codex 账号缺少 refresh token，请重新登录",
        ));
    }
    let key = format!(
        "codex:{}:{}",
        credentials.id,
        crate::flight::fingerprint(&credentials.refresh_token)
    );
    match flights().join(&key) {
        Join::Waiter(waiter) => waiter.wait().await,
        Join::Leader(leader) => {
            let result = refresh_and_save(ctx, &record, &credentials).await;
            leader.finish(result.clone());
            result
        }
    }
}

async fn refresh_and_save(
    ctx: &agent2api_provider_sdk::PluginCtx,
    record: &Value,
    previous: &CodexCredentials,
) -> Result<CodexCredentials, GatewayError> {
    let proxy = proxy_for_account(record)?;
    let (access_token, refresh_token, expiry) =
        refresh_access_token(&previous.refresh_token, &proxy).await?;
    let mut fresh = previous.clone();
    fresh.access_token = access_token;
    if let Some(refresh) = refresh_token.as_ref() {
        fresh.refresh_token = refresh.clone();
    }
    fresh.expires_at = crate::logging::now_ms() + expiry * 1000;
    let expect = crate::store::secret_view(record);
    let mut fields = vec![
        ("accessToken", Value::String(fresh.access_token.clone())),
        ("expiresAt", Value::from(fresh.expires_at)),
    ];
    if let Some(refresh) = refresh_token {
        fields.push(("refreshToken", Value::String(refresh.clone())));
        fields.push((
            "tokenTail",
            Value::String(crate::store::token_tail(&refresh)),
        ));
    }
    let next = crate::store::with_fields(&expect, &fields);
    if fresh.id.trim().is_empty() {
        return Err(GatewayError::with_status(
            409,
            "Codex 续期结果无法写回（账号记录缺 id），请重新登录",
        ));
    }
    if !crate::store::save(ctx, &fresh.id, &expect, &next).await? {
        crate::logging::verbose(
            "[Codex]",
            "token 刷新结果未写回（账号凭证期间已更新），本次请求使用刚刷出的令牌",
        );
    }
    Ok(fresh)
}
