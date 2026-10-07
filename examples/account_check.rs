//! 账号状态检查（**只做一次登录**，不创建会话、不发 completion）
//!
//! 用途：实测期间低频确认账号是否被禁言 —— 登录响应里直接带
//! `user.chat.is_muted` / `mute_until`，比跑一次对话省得多。
//!
//! 用法：`cargo run --example account_check -- -c /path/to/config.toml [email]`
//! （省略 email 时检查配置中的所有账号，账号之间串行、带间隔）

use std::time::Duration;

use ds_core::{ClientIdentity, DsClient, HifConfig, LoginPayload};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (config, _path) = ds_free_api::config::Config::load_with_args(std::env::args())?;
    // 邮箱可以是任意位置参数：`account_check <email> -c <config>` 与
    // `account_check -c <config> <email>` 都要能用 —— 否则会静默地检查配置里的
    // **所有**账号，违背「一次只用一个账号」的纪律。
    let only = std::env::args().skip(1).find(|a| a.contains('@'));

    let client = DsClient::new(
        config.ds_core.api_base.clone(),
        config.ds_core.wasm_url.clone(),
        ClientIdentity {
            user_agent: config.ds_core.user_agent.clone(),
            client_version: config.ds_core.client_version.clone(),
            client_platform: config.ds_core.client_platform.clone(),
            client_locale: config.ds_core.client_locale.clone(),
            client_bundle_id: config.ds_core.client_bundle_id.clone(),
            device_id: config.ds_core.client_device_id.clone(),
            device_model: config.ds_core.client_device_model.clone(),
            timezone_offset: config.ds_core.client_timezone_offset.clone(),
            client_os: config.ds_core.client_os.clone(),
        },
        &HifConfig {
            enabled: config.ds_core.hif_enabled,
            ..HifConfig::default()
        },
        ds_core::EmulationProfile::from_config(&config.ds_core.emulation).unwrap_or_default(),
        config.proxy.url.as_deref(),
    );

    for account in &config.ds_core.accounts {
        let id = if account.email.is_empty() {
            &account.mobile
        } else {
            &account.email
        };
        if let Some(only) = &only
            && &account.email != only
            && &account.mobile != only
        {
            continue;
        }

        let payload = LoginPayload {
            email: account.email.clone(),
            mobile: account.mobile.clone(),
            password: account.password.clone(),
            area_code: account.area_code.clone(),
            device_id: account.device_id.clone(),
            os: config.ds_core.client_os.clone(),
        };

        match client.login(&payload).await {
            Ok(data) => {
                let chat = data.user.chat.as_ref();
                let muted = chat.is_some_and(|c| c.is_muted != 0);
                println!(
                    "{}: 登录成功 is_muted={} mute_until={:?}",
                    id,
                    muted,
                    chat.and_then(|c| c.mute_until)
                );
            }
            Err(e) => println!("{id}: 登录失败 {e}"),
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    Ok(())
}
