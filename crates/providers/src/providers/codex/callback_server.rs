use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::sync::OnceLock;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use super::{endpoints, login};

pub async fn ensure_running() -> Result<u16, String> {
    static PORT: OnceLock<tokio::sync::Mutex<Option<u16>>> = OnceLock::new();
    let port = PORT.get_or_init(|| tokio::sync::Mutex::new(None));
    let mut active = port.lock().await;
    if let Some(port) = *active {
        return Ok(port);
    }

    let mut last_error = None;
    let mut bound = None;
    for candidate in [endpoints::CALLBACK_PORT, endpoints::CALLBACK_FALLBACK_PORT] {
        match TcpListener::bind((Ipv4Addr::LOCALHOST, candidate)).await {
            Ok(listener) => {
                bound = Some((candidate, listener));
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {
                last_error = Some(error);
            }
            Err(error) => {
                return Err(format!(
                    "登录无法监听回调地址 {}：{error}",
                    endpoints::redirect_uri(candidate)
                ));
            }
        }
    }
    let Some((selected_port, listener)) = bound else {
        let error = last_error
            .map(|error| error.to_string())
            .unwrap_or_else(|| "端口不可用".to_string());
        return Err(format!(
            "登录无法监听回调端口 {} 或 {}：{error}",
            endpoints::CALLBACK_PORT,
            endpoints::CALLBACK_FALLBACK_PORT
        ));
    };

    tokio::spawn(serve(listener));
    *active = Some(selected_port);
    crate::logging::log(
        "[Login]",
        &format!("登录回调已监听 {}", endpoints::redirect_uri(selected_port)),
    );
    Ok(selected_port)
}

async fn serve(listener: TcpListener) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                tokio::spawn(handle(stream));
            }
            Err(error) => {
                crate::logging::verbose("[Login]", &format!("回调连接失败：{error}"));
                return;
            }
        }
    }
}

async fn handle(mut stream: TcpStream) {
    let mut request = Vec::new();
    let mut buffer = [0u8; 2048];
    while request.len() < 32 * 1024 && !request.windows(4).any(|part| part == b"\r\n\r\n") {
        match stream.read(&mut buffer).await {
            Ok(0) => break,
            Ok(read) => request.extend_from_slice(&buffer[..read]),
            Err(_) => return,
        }
    }

    let line = String::from_utf8_lossy(&request);
    let mut fields = line.lines().next().unwrap_or("").split_whitespace();
    let method = fields.next().unwrap_or("");
    let target = fields.next().unwrap_or("");
    let (status, body) = if method == "GET" {
        callback_result(target)
    } else {
        (405, "不支持的请求方法。")
    };
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Bad Request",
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
}

fn callback_result(target: &str) -> (u16, &'static str) {
    let Ok(url) = url::Url::parse(&format!("http://localhost{target}")) else {
        return (400, "回调地址无效，请返回应用重新登录。\n");
    };
    if url.path() != endpoints::CALLBACK_PATH {
        return (404, "找不到回调地址。\n");
    }
    let params = url.query_pairs().into_owned().collect::<HashMap<_, _>>();
    if login::accept_callback(&params) {
        (200, "登录已完成，可以关闭此页面并返回应用。\n")
    } else {
        (400, "登录状态无效或已过期，请返回应用重新开始。\n")
    }
}
