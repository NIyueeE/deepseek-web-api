//! DeepSeek 核心配置 —— 独立于根 crate 的 Config
//!
//! 由根 crate 的 `Config` 构造转换而来。

/// ds_core 所需的配置（从根 crate Config 的子集构造）
#[derive(Debug, Clone)]
pub struct DsCoreConfig {
    pub api_base: String,
    pub wasm_url: String,
    pub user_agent: String,
    pub client_version: String,
    pub client_platform: String,
    pub client_locale: String,
    /// X-Client-Bundle-Id 请求头（真实客户端固定为 com.deepseek.chat）
    pub client_bundle_id: String,
    /// X-Device-Id 请求头（设备级 UUID，空 = 按 api_base 确定性派生兜底）
    pub client_device_id: String,
    /// X-Device-Model 请求头（真实 Web 客户端发空串）
    pub client_device_model: String,
    /// X-Client-Timezone-Offset 请求头（分钟或秒，真实 Web 客户端 UTC+8 发 28800）
    pub client_timezone_offset: String,
    /// 登录 payload 的 os 字段（真实 Web 客户端为 "web"，App 为 "android"）
    pub client_os: String,
    /// 传输层拟态档位（`okhttp4_12` = 原生 App，`chrome136` = 桌面 Chrome）
    pub emulation: String,
    /// 是否在 completion 请求上回传 `x-hif-leim` 风控令牌
    ///
    /// 真实客户端会轮询 `hif-leim.deepseek.com` 取令牌并在 SSE 请求上带回；
    /// 关闭仅用于对照实验（正常使用务必保持开启）。
    pub hif_enabled: bool,
    pub proxy_url: Option<String>,
    pub model_types: Vec<String>,
    pub input_character_limits: Vec<u32>,
    /// 每账号每小时请求上限（0 = 不限制）
    pub hourly_request_quota: u64,
}

/// 单个账号配置
#[derive(Debug, Clone)]
pub struct AccountConfig {
    pub email: String,
    pub mobile: String,
    pub area_code: String,
    pub password: String,
    /// 浏览器设备指纹 ID。
    ///
    /// **实测为必填**：缺失会被登录风控直接拒绝（`RISK_DEVICE_DETECTED`，biz_code 11）。
    ///
    /// 建议**每个账号使用独立的 device_id**：设备级指纹被上游用于关联与画像，
    /// 同一指纹下挂多个账号、累计数百次请求后，账号会被禁言（`biz_code=5`）。
    pub device_id: String,
}
