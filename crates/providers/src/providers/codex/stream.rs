//! Codex Responses SSE 事件到 Chat SSE 帧的转换。

use std::collections::BTreeMap;

use bytes::Bytes;
use futures::{stream::BoxStream, StreamExt};
use serde_json::{json, Value};

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::ConversationSink;

use crate::sse::{LineBuffer, SseLine};
use crate::{http, ids};
pub(super) fn error_body(status: u16, message: &str) -> Value {
    json!({
        "error": {
            "type": if status == 429 { "rate_limit_error" } else { "api_error" },
            "message": message,
        }
    })
}
fn sse(payload: &Value) -> Vec<u8> {
    format!("data: {payload}\n\n").into_bytes()
}

fn chunk(model: &str, id: &str, delta: Value, finish: Option<&str>) -> Vec<u8> {
    sse(&json!({
        "id": id,
        "object": "chat.completion.chunk",
        "created": chrono::Utc::now().timestamp(),
        "model": model,
        "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }],
    }))
}

fn usage_chunk(
    model: &str,
    id: &str,
    reason: &str,
    input: i64,
    output: i64,
    cached: i64,
) -> Vec<u8> {
    sse(&json!({
        "id": id,
        "object": "chat.completion.chunk",
        "created": chrono::Utc::now().timestamp(),
        "model": model,
        "choices": [{ "index": 0, "delta": {}, "finish_reason": reason }],
        "usage": {
            "prompt_tokens": input.max(0),
            "completion_tokens": output.max(0),
            "total_tokens": (input + output).max(0),
            "prompt_tokens_details": { "cached_tokens": cached.max(0) },
        },
    }))
}

pub(super) async fn head(sink: &mut ConversationSink<'_>, status: u16) -> Result<(), String> {
    sink.head(proto::StreamHead {
        status,
        headers: BTreeMap::from([(
            "content-type".to_string(),
            if status == 200 {
                "text/event-stream"
            } else {
                "application/json"
            }
            .to_string(),
        )]),
        upstream: proto::UpstreamKind::Chat,
    })
    .await
}

/// Responses 的 SSE 事件读取器。
struct EventStream {
    stream: BoxStream<'static, Result<Bytes, reqwest::Error>>,
    lines: LineBuffer,
    pending: std::collections::VecDeque<Value>,
    ended: bool,
    label: &'static str,
}

impl EventStream {
    fn new(response: reqwest::Response, label: &'static str) -> Self {
        Self {
            stream: Box::pin(response.bytes_stream()),
            lines: LineBuffer::new(),
            pending: std::collections::VecDeque::new(),
            ended: false,
            label,
        }
    }

    async fn next(&mut self) -> Result<Option<Value>, String> {
        loop {
            if let Some(value) = self.pending.pop_front() {
                return Ok(Some(value));
            }
            if self.ended {
                return Ok(None);
            }
            match self.stream.next().await {
                Some(Ok(bytes)) => {
                    for line in self.lines.push(&bytes) {
                        self.handle(&line);
                    }
                }
                Some(Err(error)) => {
                    return Err(format!(
                        "读取 {} 的响应失败：{}",
                        self.label,
                        http::describe(&error)
                    ))
                }
                None => {
                    self.ended = true;
                    if let Some(line) = self.lines.finish() {
                        self.handle(&line);
                    }
                }
            }
        }
    }

    fn handle(&mut self, line: &str) {
        let parsed = SseLine::parse(line);
        let Some(payload) = parsed.data() else {
            return;
        };
        if payload == "[DONE]" {
            self.ended = true;
            return;
        }
        if let Ok(value) = serde_json::from_str::<Value>(payload) {
            self.pending.push_back(value);
        }
    }
}

pub(super) async fn translate_response(
    response: reqwest::Response,
    model: &str,
    label: &'static str,
    sink: &mut ConversationSink<'_>,
) -> Result<(), (u16, String)> {
    let mut stream = EventStream::new(response, label);
    let id = format!("chatcmpl-{}", ids::new_message_id());

    // 预读：跳过 response.created 之类的元事件，第一个有内容的事件决定成败。
    let mut tool_index: i64 = -1;
    let mut tool_args_sent = false;
    let mut text_sent = false;
    let mut started = false;
    let mut usage = (0i64, 0i64, 0i64);
    loop {
        let event = match stream.next().await {
            Ok(Some(event)) => event,
            Ok(None) => {
                if !started {
                    return Err((502, format!("{label} 在开始回答前就结束了响应")));
                }
                sink.send(&usage_chunk(&model, &id, "stop", usage.0, usage.1, usage.2))
                    .await
                    .map_err(|message| (502, message))?;
                sink.send(b"data: [DONE]\n\n")
                    .await
                    .map_err(|message| (502, message))?;
                return Ok(());
            }
            Err(message) if !started => return Err((502, message)),
            Err(message) => {
                sink.send(&sse(&error_body(502, &message)))
                    .await
                    .map_err(|error| (502, error))?;
                sink.send(b"data: [DONE]\n\n")
                    .await
                    .map_err(|error| (502, error))?;
                return Ok(());
            }
        };
        let kind = event.get("type").and_then(Value::as_str).unwrap_or("");
        if kind == "response.created" || kind == "response.in_progress" {
            continue;
        }
        if kind == "error" || kind == "response.failed" {
            let message = event
                .pointer("/response/error/message")
                .and_then(Value::as_str)
                .or_else(|| event.get("message").and_then(Value::as_str))
                .or_else(|| event.pointer("/error/message").and_then(Value::as_str))
                .unwrap_or("上游返回错误")
                .to_string();
            let status = classify_message(&message);
            if !started {
                return Err((status, message));
            }
            sink.send(&sse(&error_body(status, &message)))
                .await
                .map_err(|error| (502, error))?;
            sink.send(b"data: [DONE]\n\n")
                .await
                .map_err(|error| (502, error))?;
            return Ok(());
        }
        if kind == "response.completed" || kind == "response.done" || kind == "response.incomplete"
        {
            let (input, output, cached) = super::responses::usage_of(&event);
            usage = (input, output, cached);
            let reason = if kind == "response.incomplete" {
                "length"
            } else {
                "stop"
            };
            if !started {
                head(sink, 200).await.map_err(|message| (502, message))?;
                sink.send(&chunk(
                    &model,
                    &id,
                    json!({ "role": "assistant", "content": "" }),
                    None,
                ))
                .await
                .map_err(|message| (502, message))?;
            }
            sink.send(&usage_chunk(&model, &id, reason, usage.0, usage.1, usage.2))
                .await
                .map_err(|message| (502, message))?;
            sink.send(b"data: [DONE]\n\n")
                .await
                .map_err(|message| (502, message))?;
            return Ok(());
        }
        if !started {
            head(sink, 200).await.map_err(|message| (502, message))?;
            sink.send(&chunk(
                &model,
                &id,
                json!({ "role": "assistant", "content": "" }),
                None,
            ))
            .await
            .map_err(|message| (502, message))?;
            started = true;
        }
        let mut pending = Some(event);
        while let Some(event) = pending.take() {
            let kind = event.get("type").and_then(Value::as_str).unwrap_or("");
            match kind {
                "response.output_item.added" => {
                    let item = event.get("item").cloned().unwrap_or(Value::Null);
                    if item.get("type").and_then(Value::as_str) == Some("function_call") {
                        let call_id = item
                            .get("call_id")
                            .and_then(Value::as_str)
                            .or_else(|| item.get("id").and_then(Value::as_str))
                            .unwrap_or("")
                            .to_string();
                        let name = item
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        tool_index += 1;
                        tool_args_sent = false;
                        sink.send(&chunk(
                            &model,
                            &id,
                            json!({
                                "tool_calls": [{
                                    "index": tool_index,
                                    "id": call_id,
                                    "type": "function",
                                    "function": { "name": name, "arguments": "" },
                                }]
                            }),
                            None,
                        ))
                        .await
                        .map_err(|message| (502, message))?;
                    }
                }
                "response.output_text.delta" | "response.text.delta" => {
                    if let Some(text) = event.get("delta").and_then(Value::as_str) {
                        if !text.is_empty() {
                            text_sent = true;
                            sink.send(&chunk(&model, &id, json!({ "content": text }), None))
                                .await
                                .map_err(|message| (502, message))?;
                        }
                    }
                }
                "response.output_text.done" | "response.text.done" => {
                    // 只有没收到增量时才用整段（避免重复）。
                    if !text_sent {
                        if let Some(text) = event.get("text").and_then(Value::as_str) {
                            if !text.is_empty() {
                                text_sent = true;
                                sink.send(&chunk(&model, &id, json!({ "content": text }), None))
                                    .await
                                    .map_err(|message| (502, message))?;
                            }
                        }
                    }
                }
                "response.reasoning_summary_text.delta"
                | "response.reasoning_text.delta"
                | "response.reasoning.delta" => {
                    if let Some(text) = event.get("delta").and_then(Value::as_str) {
                        if !text.is_empty() {
                            sink.send(&chunk(
                                &model,
                                &id,
                                json!({ "reasoning_content": text }),
                                None,
                            ))
                            .await
                            .map_err(|message| (502, message))?;
                        }
                    }
                }
                "response.function_call_arguments.delta" => {
                    if let Some(args) = event.get("delta").and_then(Value::as_str) {
                        if !args.is_empty() && tool_index >= 0 {
                            tool_args_sent = true;
                            sink.send(&chunk(
                                &model,
                                &id,
                                json!({
                                    "tool_calls": [{
                                        "index": tool_index,
                                        "function": { "arguments": args },
                                    }]
                                }),
                                None,
                            ))
                            .await
                            .map_err(|message| (502, message))?;
                        }
                    }
                }
                "response.function_call_arguments.done" => {
                    // 有些实现只在 done 里给完整参数；增量没来过时补一次。
                    if !tool_args_sent && tool_index >= 0 {
                        if let Some(args) = event.get("arguments").and_then(Value::as_str) {
                            if !args.is_empty() {
                                tool_args_sent = true;
                                sink.send(&chunk(
                                    &model,
                                    &id,
                                    json!({
                                        "tool_calls": [{
                                            "index": tool_index,
                                            "function": { "arguments": args },
                                        }]
                                    }),
                                    None,
                                ))
                                .await
                                .map_err(|message| (502, message))?;
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

/// 错误文案 → HTTP 状态（尽力而为）。
fn classify_message(message: &str) -> u16 {
    let lower = message.to_ascii_lowercase();
    if lower.contains("rate limit")
        || lower.contains("too many requests")
        || lower.contains("quota")
        || lower.contains("balance")
        || lower.contains("credit")
        || lower.contains("积分")
        || lower.contains("额度")
    {
        return 429;
    }
    if lower.contains("context")
        || lower.contains("too long")
        || lower.contains("maximum length")
        || lower.contains("too large")
    {
        return 400;
    }
    502
}
