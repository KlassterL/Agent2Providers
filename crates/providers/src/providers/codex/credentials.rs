use serde_json::Value;

pub const REFRESH_SKEW_MS: i64 = 5 * 60 * 1000;

#[derive(Clone, Debug)]
pub struct CodexCredentials {
    pub id: String,
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: i64,
    pub account_id: String,
    pub user_id: String,
    pub email: String,
    pub plan_type: String,
}

impl CodexCredentials {
    pub fn expiring(&self) -> bool {
        self.access_token.trim().is_empty()
            || self.expires_at <= crate::logging::now_ms() + REFRESH_SKEW_MS
    }
}

pub fn from_record(record: &Value) -> CodexCredentials {
    CodexCredentials {
        id: record
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        access_token: pick(record, &["accessToken", "access_token"]),
        refresh_token: pick(record, &["refreshToken", "refresh_token"]),
        expires_at: record
            .get("expiresAt")
            .and_then(|value| value.as_i64().or_else(|| value.as_str()?.parse().ok()))
            .unwrap_or(0),
        account_id: pick(record, &["uid", "chatgptAccountId", "chatgpt_account_id"]),
        user_id: pick(record, &["userId", "user_id"]),
        email: pick(record, &["email"]),
        plan_type: pick(record, &["planType", "plan_type"]),
    }
}

fn pick(record: &Value, keys: &[&str]) -> String {
    keys.iter()
        .filter_map(|key| record.get(*key).and_then(Value::as_str))
        .map(str::trim)
        .find(|value| !value.is_empty())
        .unwrap_or("")
        .to_string()
}
