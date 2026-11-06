use serde_json::Value;

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::{ConversationSink, PluginCtx};

use super::stream::{error_body, head};
use super::{endpoints, models, oauth};

pub async fn run(
    ctx: &PluginCtx,
    request: proto::ConversationRequest,
    mut sink: ConversationSink<'_>,
) -> Result<(), String> {
    match forward(ctx, &request, &mut sink).await {
        Ok(()) => Ok(()),
        Err((status, message)) => {
            head(&mut sink, status).await?;
            let body = serde_json::to_vec(&error_body(status, &message)).unwrap_or_default();
            sink.send(&body).await
        }
    }
}

async fn forward(
    ctx: &PluginCtx,
    request: &proto::ConversationRequest,
    sink: &mut ConversationSink<'_>,
) -> Result<(), (u16, String)> {
    let credentials = oauth::ensure_fresh(ctx, &request.account, false)
        .await
        .map_err(|error| {
            (
                u16::try_from(error.status_code).unwrap_or(502),
                error.message,
            )
        })?;
    let model = if request.wire_model.trim().is_empty() {
        request
            .body
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
    } else {
        request.wire_model.trim()
    };
    if model.is_empty() {
        return Err((400, "请求缺少 model 字段".to_string()));
    }
    if models::is_image_model(model) {
        return Err((400, "图片模型请通过图片生成或编辑接口调用".to_string()));
    }

    let mut body = super::responses::build_request(&request.body, model, true);
    body["store"] = Value::Bool(false);
    let session = if request.session.trim().is_empty() {
        crate::ids::new_conversation_id()
    } else {
        request.session.clone()
    };
    let headers =
        super::adapter::auth_headers(&credentials, "text/event-stream", Some(&session), false);
    let client = crate::http::stream_client(request.proxy.as_ref(), &ctx.env.proxy);
    let response = crate::http::post_stream(
        &client,
        endpoints::RESPONSES_URL,
        &headers,
        &body,
        "text/event-stream",
    )
    .await
    .map_err(|error| (502, error))?;
    if !response.is_success() {
        let detail = response.error_head();
        let value: Value = serde_json::from_str(&detail).unwrap_or(Value::Null);
        let message = value
            .pointer("/error/message")
            .or_else(|| value.get("message"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .filter(|message| !message.is_empty())
            .unwrap_or(detail);
        return Err((response.status, message));
    }
    let Some(response) = response.response else {
        return Err((502, "上游没有返回响应流".to_string()));
    };

    super::stream::translate_response(response, model, "Codex", sink).await
}
