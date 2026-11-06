use serde_json::{json, Value};

use super::{endpoints, models};

pub fn direct_request(image: &Value, action: &str) -> Result<(String, Value), String> {
    if !matches!(action, "generate" | "edit") {
        return Err("图片请求缺少有效的 action".to_string());
    }
    let image_model = image
        .get("model")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| models::is_image_model(value))
        .ok_or_else(|| "图片请求缺少有效的图片模型".to_string())?;
    let prompt = image
        .get("prompt")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "图片请求缺少 prompt".to_string())?;

    let mut request = json!({ "model": image_model, "prompt": prompt });
    for field in [
        "size",
        "quality",
        "background",
        "output_format",
        "input_fidelity",
        "moderation",
        "response_format",
    ] {
        if let Some(value) = image.get(field).filter(|value| !value.is_null()) {
            request[field] = value.clone();
        }
    }
    for field in ["n", "output_compression", "partial_images"] {
        if let Some(value) = image
            .get(field)
            .filter(|value| value.as_i64().is_some() || value.as_u64().is_some())
        {
            request[field] = value.clone();
        }
    }
    if let Some(stream) = image.get("stream").and_then(Value::as_bool) {
        request["stream"] = Value::Bool(stream);
    }
    if action == "edit" {
        let images = image
            .get("images")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|value| {
                value
                    .as_str()
                    .or_else(|| value.get("image_url").and_then(Value::as_str))
            })
            .map(str::trim)
            .filter(|url| !url.is_empty())
            .map(|url| json!({ "image_url": url }))
            .collect::<Vec<_>>();
        if images.is_empty() {
            return Err("图片编辑请求至少需要一个输入图像".to_string());
        }
        request["images"] = Value::Array(images);
    }
    if let Some(mask) = image
        .get("mask")
        .and_then(|value| {
            value
                .as_str()
                .or_else(|| value.get("image_url").and_then(Value::as_str))
        })
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        request["mask"] = json!({ "image_url": mask });
    }
    let url = match action {
        "generate" => endpoints::IMAGES_GENERATIONS_URL,
        "edit" => endpoints::IMAGES_EDITS_URL,
        _ => return Err("图片请求缺少有效的 action".to_string()),
    };
    Ok((url.to_string(), request))
}
