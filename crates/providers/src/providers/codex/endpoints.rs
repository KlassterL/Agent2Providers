pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const AUTH_URL: &str = "https://auth.openai.com/oauth/authorize";
pub const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
pub const RESPONSES_URL: &str = "https://chatgpt.com/backend-api/codex/responses";
pub const IMAGES_GENERATIONS_URL: &str = "https://chatgpt.com/backend-api/codex/images/generations";
pub const IMAGES_EDITS_URL: &str = "https://chatgpt.com/backend-api/codex/images/edits";
pub const CALLBACK_PATH: &str = "/auth/callback";
pub const CALLBACK_PORT: u16 = 1455;
pub const CALLBACK_FALLBACK_PORT: u16 = 1457;
pub const SCOPES: &str = "openid email profile offline_access";
pub const ORIGINATOR: &str = "codex_cli_rs";
pub const USER_AGENT: &str = "codex_cli_rs/0.1.0";
pub const REQUEST_TIMEOUT_MS: u64 = 30_000;

pub fn redirect_uri(port: u16) -> String {
    format!("http://127.0.0.1:{port}{CALLBACK_PATH}")
}
