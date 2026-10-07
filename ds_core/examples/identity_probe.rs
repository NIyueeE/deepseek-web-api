//! 客户端身份变体的 WAF 兼容性探测
//!
//! 只打**无鉴权**的 `GET /client/settings`（真实客户端每次开页面都会打），
//! 不产生任何账号流量。用来回答：现在的「安卓 App UA + Chrome TLS 指纹」
//! 混合身份，是否需要换成自洽的 Web 身份（Chrome UA + Chrome TLS + web 头）。
//!
//! 用法：`cargo run -p ds_core --example identity_probe`

use std::time::Duration;

use wreq_util::Emulation;

const UA_ANDROID: &str = "DeepSeek/2.5.0 Android/35";
const UA_CHROME: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36";
const DID: &str = "a30f40c5-618e-49ea-bfe5-553c172ed337";

#[derive(Clone, Copy)]
enum Platform {
    Android,
    Web,
}

fn base_headers(platform: Platform, ua: &str) -> Vec<(&'static str, String)> {
    vec![
        ("user-agent", ua.to_string()),
        ("x-client-version", "2.5.0".to_string()),
        (
            "x-client-platform",
            match platform {
                Platform::Android => "android",
                Platform::Web => "web",
            }
            .to_string(),
        ),
        ("x-client-locale", "zh_CN".to_string()),
        ("x-client-bundle-id", "com.deepseek.chat".to_string()),
        ("x-device-id", DID.to_string()),
        ("x-device-model", String::new()),
        ("x-client-timezone-offset", "28800".to_string()),
    ]
}

async fn probe(name: &str, platform: Platform, ua: &str, emulation: Emulation) {
    let client = match wreq::Client::builder().emulation(emulation).build() {
        Ok(c) => c,
        Err(e) => {
            println!("{name:38} 构建客户端失败: {e}");
            return;
        }
    };
    let mut req = client
        .get("https://chat.deepseek.com/api/v0/client/settings")
        .query(&[("did", DID), ("scope", "main")])
        .timeout(Duration::from_secs(15));
    for (k, v) in base_headers(platform, ua) {
        req = req.header(k, v);
    }
    match req.send().await {
        Ok(resp) => {
            let status = resp.status().as_u16();
            let waf = resp
                .headers()
                .get("x-amzn-waf-action")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("-")
                .to_string();
            let body = resp.text().await.unwrap_or_default();
            let head: String = body.chars().take(90).collect();
            println!("{name:38} status={status} waf={waf} body={head}");
        }
        Err(e) => println!("{name:38} 请求错误: {e}"),
    }
}

/// 用一次性假凭据打登录端点：能拿到业务错误码（而非 202 challenge）
/// 就说明该身份可以通过 WAF 到达应用层
async fn probe_login(name: &str, platform: Platform, ua: &str, emulation: Emulation) {
    let client = match wreq::Client::builder().emulation(emulation).build() {
        Ok(c) => c,
        Err(e) => {
            println!("{name:38} 构建客户端失败: {e}");
            return;
        }
    };
    let body = serde_json::json!({
        "email": "waf-probe-does-not-exist@example.invalid",
        "mobile": "",
        "password": "not-a-real-password",
        "area_code": "",
        "device_id": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        "os": match platform { Platform::Android => "android", Platform::Web => "web" },
    });
    let mut req = client
        .post("https://chat.deepseek.com/api/v0/users/login")
        .timeout(Duration::from_secs(15))
        .json(&body);
    for (k, v) in base_headers(platform, ua) {
        req = req.header(k, v);
    }
    match req.send().await {
        Ok(resp) => {
            let status = resp.status().as_u16();
            let waf = resp
                .headers()
                .get("x-amzn-waf-action")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("-")
                .to_string();
            let text = resp.text().await.unwrap_or_default();
            let head: String = text.chars().take(120).collect();
            println!("{name:38} status={status} waf={waf} body={head}");
        }
        Err(e) => println!("{name:38} 请求错误: {e}"),
    }
}

/// 用**无效 token**打需要鉴权的端点：能拿到业务错误码（40003 Authorization Failed）
/// 就说明该身份通过了 WAF；返回 202 则是 WAF challenge
async fn probe_authed(name: &str, platform: Platform, ua: &str, emulation: Emulation) {
    let client = match wreq::Client::builder().emulation(emulation).build() {
        Ok(c) => c,
        Err(e) => {
            println!("{name:38} 构建客户端失败: {e}");
            return;
        }
    };
    for (path, body) in [
        (
            "/api/v0/chat/create_pow_challenge",
            serde_json::json!({"target_path": "/api/v0/chat/completion"}),
        ),
        ("/api/v0/chat_session/create", serde_json::json!({})),
    ] {
        let mut req = client
            .post(format!("https://chat.deepseek.com{path}"))
            .header("authorization", "Bearer invalid-probe-token")
            .timeout(Duration::from_secs(15))
            .json(&body);
        for (k, v) in base_headers(platform, ua) {
            req = req.header(k, v);
        }
        match req.send().await {
            Ok(resp) => {
                let status = resp.status().as_u16();
                let waf = resp
                    .headers()
                    .get("x-amzn-waf-action")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("-")
                    .to_string();
                let text = resp.text().await.unwrap_or_default();
                let head: String = text.chars().take(80).collect();
                println!("{name:38} {path:34} status={status} waf={waf} body={head}");
            }
            Err(e) => println!("{name:38} {path:34} 请求错误: {e}"),
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    // 当前实现：安卓 App 身份 + Chrome136 TLS 指纹（UA 与 sec-ch-ua 并不自洽）
    probe(
        "settings: android-app + Chrome136",
        Platform::Android,
        UA_ANDROID,
        Emulation::Chrome136,
    )
    .await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    // 全 Web 自洽身份：桌面 Chrome UA + Chrome TLS + Chrome client hints
    probe(
        "settings: web-chrome + Chrome136",
        Platform::Web,
        UA_CHROME,
        Emulation::Chrome136,
    )
    .await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    // 安卓 App 身份 + OkHttp TLS 指纹（原生 App 的真实组合）
    probe(
        "settings: android-app + OkHttp4.12",
        Platform::Android,
        UA_ANDROID,
        Emulation::OkHttp4_12,
    )
    .await;

    // 登录端点：用一次性假凭据，不涉及任何真实账号
    tokio::time::sleep(Duration::from_secs(3)).await;
    probe_login(
        "login: android-app + Chrome136",
        Platform::Android,
        UA_ANDROID,
        Emulation::Chrome136,
    )
    .await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    probe_login(
        "login: web-chrome + Chrome136",
        Platform::Web,
        UA_CHROME,
        Emulation::Chrome136,
    )
    .await;

    // 需鉴权端点（无效 token）：三种身份哪几种能过 WAF 到达应用层
    tokio::time::sleep(Duration::from_secs(3)).await;
    probe_authed(
        "authed: android-app + Chrome136",
        Platform::Android,
        UA_ANDROID,
        Emulation::Chrome136,
    )
    .await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    probe_authed(
        "authed: web-chrome + Chrome136",
        Platform::Web,
        UA_CHROME,
        Emulation::Chrome136,
    )
    .await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    probe_authed(
        "authed: android-app + OkHttp4.12",
        Platform::Android,
        UA_ANDROID,
        Emulation::OkHttp4_12,
    )
    .await;
}
