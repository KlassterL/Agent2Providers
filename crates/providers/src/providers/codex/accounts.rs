use serde_json::{json, Value};

use agent2api_provider_protocol as proto;

use super::{credentials::CodexCredentials, PROVIDER_ID};

pub fn normalize(input: proto::NormalizeInput) -> Result<proto::NormalizedAccount, String> {
    match input.source {
        proto::NormalizeSource::Manual { .. } => Err("Codex 账号请使用网页登录添加".to_string()),
        proto::NormalizeSource::Desktop => {
            Err("Codex 不支持导入桌面端登录态，请使用网页登录".to_string())
        }
        proto::NormalizeSource::FileText { .. } | proto::NormalizeSource::FileBase64 { .. } => {
            Err("Codex 没有可导入的凭证文件，请使用网页登录".to_string())
        }
    }
}

pub fn account_of(credentials: &CodexCredentials, source: &str) -> proto::NormalizedAccount {
    let token_tail = crate::store::token_tail(&credentials.refresh_token);
    let name = if credentials.email.trim().is_empty() {
        format!("Codex {token_tail}")
    } else {
        credentials.email.clone()
    };
    let secret = json!({
        "accessToken": credentials.access_token,
        "refreshToken": credentials.refresh_token,
        "expiresAt": credentials.expires_at,
        "uid": credentials.account_id,
        "userId": credentials.user_id,
        "email": credentials.email,
        "planType": credentials.plan_type,
        "tokenTail": token_tail,
        "source": source,
        "name": name,
    });
    let public = account_view(&secret);
    proto::NormalizedAccount {
        secret,
        public,
        identity: credentials.account_id.clone(),
        desktop: false,
    }
}

pub fn account_view(secret: &Value) -> Value {
    let access_token = crate::store::pick(secret, &["accessToken", "access_token"]);
    let refresh_token = crate::store::pick(secret, &["refreshToken", "refresh_token"]);
    let mut public = json!({
        "provider": PROVIDER_ID,
        "name": crate::store::pick(secret, &["name", "email"]),
        "uid": crate::store::pick(secret, &["uid", "chatgptAccountId", "chatgpt_account_id"]),
        "userId": crate::store::pick(secret, &["userId", "user_id"]),
        "email": crate::store::pick(secret, &["email"]),
        "planType": crate::store::pick(secret, &["planType", "plan_type"]),
        "tokenTail": crate::store::token_tail(&refresh_token),
        "hasAccessToken": !access_token.is_empty(),
        "hasRefreshToken": !refresh_token.is_empty(),
        "desktop": false,
        "available": !refresh_token.is_empty(),
    });
    if let Some(expires_at) = secret
        .get("expiresAt")
        .and_then(|value| value.as_i64().or_else(|| value.as_str()?.parse().ok()))
        .filter(|value| *value > 0)
    {
        public["expiresAt"] = Value::from(expires_at);
    }
    public
}

pub fn identity_of(secret: &Value) -> String {
    crate::store::pick(secret, &["uid", "chatgptAccountId", "chatgpt_account_id"])
}
