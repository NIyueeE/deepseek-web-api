//! 提示词格式探针 —— 绕过 OpenAI 适配层，直接向 ds_core 发送任意 prompt
//!
//! 用于 A/B 对比不同提示词注入方式（原生 chatml vs `<think>` reminder 注入）
//! 对模型遵循度的影响，是 `docs/deepseek-prompt-injection.md` 所述
//! 「实验驱动、增量维护」策略的配套工具。
//!
//! 用法:
//!   PROBE_VARIANTS=variants.json PROBE_MODEL_TYPE=default \
//!     cargo run --example prompt_probe -- -c py-e2e-tests/config.toml
//!
//! variants.json 格式: `[{"label": "变体名", "prompt": "完整 prompt 字符串"}]`

use std::time::Instant;

use ds_core::{AccountConfig, ChatRequest, DsCore, DsCoreConfig, StreamEvent};
use ds_free_api::Config;
use futures::StreamExt;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::new().default_filter_or("warn")).init();

    let variants_path =
        std::env::var("PROBE_VARIANTS").expect("需要设置 PROBE_VARIANTS 指向变体 JSON 文件");
    let model_type = std::env::var("PROBE_MODEL_TYPE").unwrap_or_else(|_| "default".into());

    let (config, _config_path) = Config::load_with_args(std::env::args())?;

    let core_cfg = DsCoreConfig {
        api_base: config.ds_core.api_base.clone(),
        wasm_url: config.ds_core.wasm_url.clone(),
        user_agent: config.ds_core.user_agent.clone(),
        client_version: config.ds_core.client_version.clone(),
        client_platform: config.ds_core.client_platform.clone(),
        client_locale: config.ds_core.client_locale.clone(),
        client_bundle_id: config.ds_core.client_bundle_id.clone(),
        client_device_id: config.ds_core.client_device_id.clone(),
        client_device_model: config.ds_core.client_device_model.clone(),
        client_timezone_offset: config.ds_core.client_timezone_offset.clone(),
        client_os: config.ds_core.client_os.clone(),
        hif_enabled: config.ds_core.hif_enabled,
        proxy_url: config.proxy.url.clone(),
        model_types: config.ds_core.model_types.clone(),
        input_character_limits: config.ds_core.input_character_limits.clone(),
        // 探测工具会连续发请求做 A/B 对比，配额交给调用方自行控制
        hourly_request_quota: 0,
    };
    let accounts: Vec<AccountConfig> = config
        .ds_core
        .accounts
        .iter()
        .map(|a| AccountConfig {
            email: a.email.clone(),
            mobile: a.mobile.clone(),
            area_code: a.area_code.clone(),
            password: a.password.clone(),
            device_id: a.device_id.clone(),
        })
        .collect();

    let core = DsCore::new(&core_cfg, accounts).await?;

    let variants: Vec<serde_json::Value> = serde_json::from_slice(&std::fs::read(&variants_path)?)?;

    println!("模型类型: {model_type} | 变体数: {}", variants.len());

    for (i, v) in variants.iter().enumerate() {
        let label = v["label"].as_str().unwrap_or("?");
        let prompt = v["prompt"].as_str().unwrap_or("");
        println!(
            "\n===== [{}] {} ({} chars) =====",
            i + 1,
            label,
            prompt.chars().count()
        );

        let req = ChatRequest {
            prompt: prompt.to_string(),
            thinking_enabled: false,
            search_enabled: false,
            model_type: model_type.clone(),
            files: vec![],
        };

        let start = Instant::now();
        match core.v0_chat(req, &format!("probe-{i}")).await {
            Ok(resp) => {
                let mut stream = resp.stream;
                let mut content = String::new();
                let mut thinking = String::new();
                let mut finish = None;
                let mut usage = None;

                while let Some(ev) = stream.next().await {
                    match ev {
                        Ok(StreamEvent::ThinkDelta { content: c }) => thinking.push_str(&c),
                        Ok(StreamEvent::ContentDelta { content: c }) => content.push_str(&c),
                        Ok(StreamEvent::Done {
                            finish_reason,
                            accumulated_token_usage,
                        }) => {
                            finish = finish_reason;
                            usage = accumulated_token_usage;
                        }
                        Ok(_) => {}
                        Err(e) => {
                            println!("  STREAM ERROR: {e}");
                            break;
                        }
                    }
                }

                println!("  elapsed : {:?}", start.elapsed());
                println!("  finish  : {finish:?}  usage: {usage:?}");
                println!("  think   : {}", truncate(&thinking, 160));
                println!("  content : {}", truncate(&content, 500));
            }
            Err(e) => println!("  REQUEST ERROR: {e}"),
        }
    }

    core.shutdown().await;
    Ok(())
}

fn truncate(s: &str, n: usize) -> String {
    let t: String = s.chars().take(n).collect();
    if s.chars().count() > n {
        format!("{t}…")
    } else {
        t
    }
}
