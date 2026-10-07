//! HIF 风控令牌端点连通性探测（不涉及任何账号流量）
//!
//! 真实客户端在应用启动时就会轮询该端点，因此这里单独探测它是否会走通
//! WAF / 网络路径，避免把账号流量浪费在一个取不到令牌的构建上。
//!
//! 用法：`cargo run --example hif_probe`

use ds_core::{ClientIdentity, DsClient, HifConfig};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = DsClient::new(
        "https://chat.deepseek.com/api/v0".to_string(),
        "https://fe-static.deepseek.com/chat/static/sha3_wasm_bg.7b9ca65ddd.wasm".to_string(),
        ClientIdentity {
            user_agent: "DeepSeek/2.5.0 Android/35".to_string(),
            client_version: "2.5.0".to_string(),
            client_platform: "android".to_string(),
            client_locale: "zh_CN".to_string(),
            client_bundle_id: "com.deepseek.chat".to_string(),
            device_id: String::new(),
            device_model: String::new(),
            timezone_offset: "28800".to_string(),
            client_os: "android".to_string(),
        },
        &HifConfig::default(),
        ds_core::EmulationProfile::default(),
        None,
    );

    println!("warm_up_hif() ...");
    client.warm_up_hif().await;
    match client.hif_token().await {
        Some(token) => println!(
            "OK: x-hif-leim = {}…{}（长度 {}）",
            &token[..token.len().min(12)],
            &token[token.len().saturating_sub(8)..],
            token.len()
        ),
        None => println!("FAILED: 未取到 x-hif-leim（见 RUST_LOG=ds_core::client=debug 日志）"),
    }
    Ok(())
}
