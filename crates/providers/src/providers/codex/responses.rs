//! Codex Chat 请求到 Responses 请求体的转换。
use serde_json::{json, Value};

/// 文本内容（字符串 / 内容块数组）。
fn content_text(content: &Value) -> String {
    if let Some(text) = content.as_str() {
        return text.to_string();
    }
    let Some(parts) = content.as_array() else {
        return String::new();
    };
    parts
        .iter()
        .filter_map(|part| match part.get("type").and_then(Value::as_str) {
            Some("text" | "input_text" | "output_text") => {
                part.get("text").and_then(Value::as_str).map(str::to_string)
            }
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// 用户消息的 content 数组（文本 + 图片）。
fn user_content(message: &Value) -> Value {
    let content = message.get("content").cloned().unwrap_or(Value::Null);
    if let Some(text) = content.as_str() {
        return json!([{ "type": "input_text", "text": text }]);
    }
    let Some(parts) = content.as_array() else {
        return json!([{ "type": "input_text", "text": "" }]);
    };
    let mut items: Vec<Value> = Vec::new();
    for part in parts {
        match part.get("type").and_then(Value::as_str) {
            Some("text" | "input_text") => {
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    items.push(json!({ "type": "input_text", "text": text }));
                }
            }
            Some("image_url") => {
                if let Some(url) = part.pointer("/image_url/url").and_then(Value::as_str) {
                    items.push(json!({ "type": "input_image", "image_url": url }));
                }
            }
            Some("input_image") => {
                if let Some(url) = part.get("image_url").and_then(Value::as_str) {
                    items.push(json!({ "type": "input_image", "image_url": url }));
                }
            }
            _ => {}
        }
    }
    if items.is_empty() {
        items.push(json!({ "type": "input_text", "text": "" }));
    }
    Value::Array(items)
}

/// 把 OpenAI Chat 的 `messages` 折成 Responses 的 `input`。
fn build_input(messages: &[Value]) -> Vec<Value> {
    let mut input: Vec<Value> = Vec::new();
    for message in messages {
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("user");
        match role {
            "system" | "developer" => {
                // 交给 `instructions`；这里跳过（见 build_request）。
            }
            "assistant" => {
                let text = content_text(message.get("content").unwrap_or(&Value::Null));
                if !text.trim().is_empty() {
                    input.push(json!({
                        "type": "message",
                        "role": "assistant",
                        "content": [{ "type": "output_text", "text": text }],
                    }));
                }
                for call in message
                    .get("tool_calls")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    let call_id = call.get("id").and_then(Value::as_str).unwrap_or("");
                    let name = call
                        .pointer("/function/name")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let arguments = call
                        .pointer("/function/arguments")
                        .and_then(Value::as_str)
                        .unwrap_or("{}");
                    if name.is_empty() {
                        continue;
                    }
                    input.push(json!({
                        "type": "function_call",
                        "call_id": call_id,
                        "name": name,
                        "arguments": arguments,
                    }));
                }
            }
            "tool" => {
                let call_id = message
                    .get("tool_call_id")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let output = content_text(message.get("content").unwrap_or(&Value::Null));
                input.push(json!({
                    "type": "function_call_output",
                    "call_id": call_id,
                    "output": if output.is_empty() { "(no output)".to_string() } else { output },
                }));
            }
            _ => {
                input.push(json!({
                    "type": "message",
                    "role": "user",
                    "content": user_content(message),
                }));
            }
        }
    }
    input
}

/// 工具声明：Chat 的 `{type:function, function:{…}}` → Responses 的扁平形态。
fn build_tools(tools: Option<&Value>) -> Option<Vec<Value>> {
    let list = tools?.as_array()?;
    let converted: Vec<Value> = list
        .iter()
        .filter_map(|tool| {
            let function = tool.get("function").unwrap_or(tool);
            let name = function.get("name").and_then(Value::as_str)?;
            if name.is_empty() {
                return None;
            }
            Some(json!({
                "type": "function",
                "name": name,
                "description": function
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or(""),
                "parameters": function
                    .get("parameters")
                    .cloned()
                    .unwrap_or_else(|| json!({ "type": "object", "properties": {} })),
                "strict": false,
            }))
        })
        .collect();
    (!converted.is_empty()).then_some(converted)
}

/// `tool_choice` 的形态转换：Chat 的 `{type:function, function:{name}}`
/// 与 Responses 的 `{type:function, name}` 不是一回事，原样透传会被拒。
fn convert_tool_choice(choice: &Value) -> Value {
    match choice.as_str() {
        Some(mode) if mode == "auto" || mode == "none" || mode == "required" => {
            json!(mode)
        }
        _ => {
            if choice.get("type").and_then(Value::as_str) == Some("function") {
                if let Some(name) = choice
                    .pointer("/function/name")
                    .and_then(Value::as_str)
                    .or_else(|| choice.get("name").and_then(Value::as_str))
                {
                    return json!({ "type": "function", "name": name });
                }
            }
            choice.clone()
        }
    }
}

/// 构造 Responses 请求体。
pub fn build_request(body: &Value, model: &str, stream: bool) -> Value {
    let messages = body
        .get("messages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let instructions = messages
        .iter()
        .filter(|message| {
            matches!(
                message.get("role").and_then(Value::as_str),
                Some("system" | "developer")
            )
        })
        .map(|message| content_text(message.get("content").unwrap_or(&Value::Null)))
        .filter(|text| !text.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    let mut request = json!({
        "model": model,
        "input": build_input(&messages),
        "stream": stream,
    });
    if !instructions.is_empty() {
        request["instructions"] = json!(instructions);
    }
    if let Some(tools) = build_tools(body.get("tools")) {
        request["tools"] = json!(tools);
        if let Some(choice) = body.get("tool_choice") {
            request["tool_choice"] = convert_tool_choice(choice);
        }
        if let Some(parallel) = body.get("parallel_tool_calls") {
            request["parallel_tool_calls"] = parallel.clone();
        }
    }
    for key in ["max_output_tokens", "temperature", "top_p"] {
        if let Some(value) = body.get(key).filter(|value| !value.is_null()) {
            request[key] = value.clone();
        }
    }
    if request.get("max_output_tokens").is_none() {
        if let Some(value) = body.get("max_tokens").filter(|value| !value.is_null()) {
            request["max_output_tokens"] = value.clone();
        }
    }
    let effort = body
        .get("reasoning_effort")
        .and_then(Value::as_str)
        .or_else(|| body.pointer("/reasoning/effort").and_then(Value::as_str))
        .unwrap_or("");
    if !effort.is_empty() && effort != "none" {
        request["reasoning"] = json!({ "effort": effort });
    }
    if let Some(format) = body.get("response_format") {
        let kind = format.get("type").and_then(Value::as_str).unwrap_or("");
        if kind == "json_object" {
            request["text"] = json!({ "format": { "type": "json_object" } });
        } else if kind == "json_schema" {
            let schema = format.get("json_schema").cloned().unwrap_or(Value::Null);
            let name = schema
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("output");
            request["text"] = json!({
                "format": {
                    "type": "json_schema",
                    "name": name,
                    "schema": schema.get("schema").cloned().unwrap_or(Value::Null),
                    "strict": schema.get("strict").and_then(Value::as_bool).unwrap_or(false),
                }
            });
        }
    }
    request
}

/// Responses 的用量字段 → chat 的用量数字。
pub fn usage_of(value: &Value) -> (i64, i64, i64) {
    let usage = value
        .get("usage")
        .or_else(|| value.pointer("/response/usage"))
        .cloned()
        .unwrap_or(Value::Null);
    let input = usage
        .get("input_tokens")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let output = usage
        .get("output_tokens")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let cached = usage
        .pointer("/input_tokens_details/cached_tokens")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    (input, output, cached)
}

#[cfg(test)]
mod tests {
    use super::build_request;
    use serde_json::json;

    #[test]
    fn text_messages_use_codex_message_content_type() {
        let request = build_request(
            &json!({
                "messages": [
                    {"role": "user", "content": "probe"},
                    {"role": "assistant", "content": "ack"},
                ],
            }),
            "gpt-6-luna",
            true,
        );

        assert_eq!(request["input"][0]["type"], "message");
        assert_eq!(request["input"][0]["content"][0]["type"], "input_text");
        assert_eq!(request["input"][1]["type"], "message");
        assert_eq!(request["input"][1]["content"][0]["type"], "output_text");
    }
}
