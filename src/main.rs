//! ai-free-api 主入口 —— 启动 HTTP 服务器

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let data_dir = std::env::var("DS_DATA_DIR").unwrap_or_else(|_| ".".to_string());
    let log_path = format!("{data_dir}/logs/runtime.log");
    ds_free_api::server::runtime_log::init(&log_path);
    let (config, config_path) = ds_free_api::Config::load_with_args(std::env::args())?;
    ds_free_api::server::run(config, config_path).await
}
