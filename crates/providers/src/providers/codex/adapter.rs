use serde_json::Value;

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::images::{ImagePlan, ImageRequest};
use agent2api_provider_sdk::{ConversationSink, PluginCtx, Provider, ProviderMeta, SendFuture};

use super::{accounts, credentials, endpoints, images, login, models, oauth, PROVIDER_ID};

#[derive(Clone, Copy)]
pub struct CodexProvider;

impl Provider for CodexProvider {
    fn meta(&self) -> ProviderMeta {
        ProviderMeta::new(PROVIDER_ID, "Codex")
            .with_capabilities(proto::Capabilities {
                chat: true,
                stateful: true,
                refresh: true,
                model_refresh: false,
                usage: false,
                checkin: false,
                claims: false,
                web_login: true,
                sse_model_rewrite: false,
                sse_strip_newline_chunks: false,
                anonymous_default_session: false,
                system_proxy_when_unset: true,
                refresh_uses_account: true,
                supports_default_model: false,
                env_credentials: false,
            })
            .with_login(vec![proto::LoginMethod::Browser {
                button: "Codex 登录".to_string(),
                hint: Some("登录完成后会自动加入账号列表。".to_string()),
            }])
            .with_models(models::list())
            .with_account_features(proto::AccountFeatures {
                identifier: vec!["uid".to_string()],
                ..Default::default()
            })
    }

    fn provider_id(&self) -> String {
        PROVIDER_ID.to_string()
    }

    fn conversation<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        req: proto::ConversationRequest,
        sink: ConversationSink<'a>,
    ) -> SendFuture<'a, ()> {
        Box::pin(super::chat::run(ctx, req, sink))
    }

    fn supports_image_generation(&self) -> bool {
        true
    }

    fn supports_image_editing(&self) -> bool {
        true
    }

    fn build_image<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        req: ImageRequest,
    ) -> SendFuture<'a, ImagePlan> {
        Box::pin(async move { build_image_plan(ctx, req, "generate").await })
    }

    fn build_image_edit<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        req: ImageRequest,
    ) -> SendFuture<'a, ImagePlan> {
        Box::pin(async move { build_image_plan(ctx, req, "edit").await })
    }

    fn ensure_token<'a>(&'a self, ctx: &'a PluginCtx, account: &'a str) -> SendFuture<'a, String> {
        Box::pin(async move {
            let fresh = oauth::ensure_fresh(ctx, account, false)
                .await
                .map_err(|error| error.message)?;
            if fresh.access_token.trim().is_empty() {
                return Err("Codex 账号没有可用的 access token，请重新登录".to_string());
            }
            Ok(fresh.access_token)
        })
    }

    fn refresh_token<'a>(&'a self, ctx: &'a PluginCtx, account: &'a str) -> SendFuture<'a, String> {
        Box::pin(async move {
            oauth::ensure_fresh(ctx, account, true)
                .await
                .map(|fresh| fresh.access_token)
                .map_err(|error| error.message)
        })
    }

    fn token_expiring<'a>(&'a self, ctx: &'a PluginCtx, account: &'a str) -> SendFuture<'a, bool> {
        Box::pin(async move {
            if account.trim().is_empty() {
                return Ok(false);
            }
            let record = match crate::store::record(ctx, PROVIDER_ID, account).await {
                Ok(record) => record,
                Err(_) => return Ok(false),
            };
            let credentials = credentials::from_record(&record);
            Ok(!credentials.refresh_token.trim().is_empty() && credentials.expiring())
        })
    }

    fn normalize_account<'a>(
        &'a self,
        _ctx: &'a PluginCtx,
        input: proto::NormalizeInput,
    ) -> SendFuture<'a, proto::NormalizedAccount> {
        Box::pin(async move { accounts::normalize(input) })
    }

    fn account_view(&self, secret: &Value) -> Result<proto::AccountView, String> {
        Ok(accounts::account_view(secret))
    }

    fn account_identity(&self, secret: &Value) -> Result<String, String> {
        Ok(accounts::identity_of(secret))
    }

    fn admin_action<'a>(
        &'a self,
        _ctx: &'a PluginCtx,
        _account: &'a str,
        action: &'a str,
        _payload: &'a Value,
    ) -> SendFuture<'a, Value> {
        Box::pin(async move {
            match action {
                "auth.methods" => Ok(serde_json::json!({
                    "methods": [{
                        "methodIndex": 0,
                        "type": "oauth",
                        "label": "Codex 网页登录",
                        "instructions": "登录完成后会自动添加账号；远程访问时如未自动回调，请粘贴浏览器最终地址。",
                        "prompts": [],
                        "localOnly": false,
                        "manualCallback": true,
                        "callbackInput": "url"
                    }]
                })),
                other => Err(agent2api_provider_sdk::unsupported(format!(
                    "Codex 不支持管理动作「{other}」"
                ))),
            }
        })
    }

    fn login_start<'a>(
        &'a self,
        _ctx: &'a PluginCtx,
        method: &'a str,
        payload: &'a Value,
    ) -> SendFuture<'a, proto::LoginStart> {
        Box::pin(login::login_start(method, payload))
    }

    fn login_poll<'a>(
        &'a self,
        _ctx: &'a PluginCtx,
        flow: &'a str,
    ) -> SendFuture<'a, proto::LoginPoll> {
        Box::pin(login::login_poll(flow))
    }

    fn login_callback<'a>(
        &'a self,
        _ctx: &'a PluginCtx,
        callback: proto::LoginCallback,
    ) -> SendFuture<'a, proto::LoginPoll> {
        Box::pin(login::login_callback(callback))
    }

    fn login_cancel<'a>(&'a self, _ctx: &'a PluginCtx, flow: &'a str) -> SendFuture<'a, ()> {
        Box::pin(login::login_cancel(flow))
    }

    fn classify(&self, status: u16, body: &Value) -> proto::ErrorClass {
        let raw = body
            .pointer("/error/message")
            .or_else(|| body.get("message"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("上游错误");
        let message = format!("上游返回 {status}: {raw}");
        let code = body
            .pointer("/error/code")
            .or_else(|| body.get("code"))
            .and_then(Value::as_i64);
        let lowered = raw.to_ascii_lowercase();
        if status == 401 || status == 403 {
            return proto::ErrorClass::TokenExpired { message };
        }
        if status == 429
            || ["rate_limit", "quota", "capacity"]
                .iter()
                .any(|part| lowered.contains(part))
        {
            return proto::ErrorClass::QuotaLimited {
                reset_at: None,
                message,
                upstream_code: code,
                status: 429,
            };
        }
        if status == 400 {
            return crate::content_block::classify_or_fatal(status, body, message, code);
        }
        proto::ErrorClass::Fatal {
            status,
            message,
            upstream_code: code,
        }
    }
}

async fn build_image_plan(
    ctx: &PluginCtx,
    req: ImageRequest,
    action: &str,
) -> Result<ImagePlan, String> {
    let credentials = oauth::ensure_fresh(ctx, &req.account, false)
        .await
        .map_err(|error| error.message)?;
    let (url, mut body) = images::direct_request(&req.body, action)?;
    body["stream"] = Value::Bool(false);
    let headers = auth_headers(
        &credentials,
        "application/json",
        Some(&crate::ids::new_conversation_id()),
        true,
    );
    Ok(ImagePlan::openai(url, headers, body))
}

pub(super) fn auth_headers(
    credentials: &credentials::CodexCredentials,
    accept: &str,
    session: Option<&str>,
    image: bool,
) -> Vec<(String, String)> {
    let mut headers = vec![
        (
            "Authorization".to_string(),
            format!("Bearer {}", credentials.access_token),
        ),
        (
            "ChatGPT-Account-Id".to_string(),
            credentials.account_id.clone(),
        ),
        ("Content-Type".to_string(), "application/json".to_string()),
        ("Accept".to_string(), accept.to_string()),
        ("Originator".to_string(), endpoints::ORIGINATOR.to_string()),
        ("User-Agent".to_string(), endpoints::USER_AGENT.to_string()),
    ];
    if let Some(session) = session {
        headers.push(("Session-Id".to_string(), session.to_string()));
    }
    if image {
        headers.push((
            "x-codex-image-turn-id".to_string(),
            crate::ids::new_conversation_id(),
        ));
    }
    headers
}
