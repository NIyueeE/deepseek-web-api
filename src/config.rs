//! 配置加载模块 —— 统一配置入口
//!
//! 支持 `-c <path>` 命令行参数，默认值见下方函数。
//! config.toml 中注释项使用代码默认值。

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// 应用配置根结构
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    /// DeepSeek 核心配置（账号、客户端、模型等）
    pub ds_core: DsCoreSection,
    /// HTTP 服务器配置（必填）
    pub server: ServerConfig,
    /// 代理配置（可选，用于绕过 WAF）
    #[serde(default)]
    pub proxy: ProxyConfig,
    /// Admin 配置（bcrypt 密码哈希、JWT 密钥等，由管理面板管理）
    #[serde(default)]
    pub admin: AdminConfig,
    /// API Key 列表（由管理面板管理）
    #[serde(default)]
    pub api_keys: Vec<ApiKeyEntry>,
}

/// DeepSeek 核心配置段 —— 对应 config.toml 的 [ds_core]
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DsCoreSection {
    /// 账号池（必需，可为空——启动后通过管理面板添加）
    #[serde(default)]
    pub accounts: Vec<Account>,
    /// API 基础地址
    #[serde(default = "default_api_base")]
    pub api_base: String,
    /// WASM 文件完整 URL（PoW 计算所需，版本号可能变动）
    #[serde(default = "default_wasm_url")]
    pub wasm_url: String,
    /// User-Agent 请求头
    #[serde(default = "default_user_agent")]
    pub user_agent: String,
    /// X-Client-Version 请求头（用于 expert 模型等功能）
    #[serde(default = "default_client_version")]
    pub client_version: String,
    /// X-Client-Platform 请求头
    #[serde(default = "default_client_platform")]
    pub client_platform: String,
    /// X-Client-Locale 请求头
    #[serde(default = "default_client_locale")]
    pub client_locale: String,
    /// X-Client-Bundle-Id 请求头（真实客户端固定 com.deepseek.chat）
    #[serde(default = "default_client_bundle_id")]
    pub client_bundle_id: String,
    /// X-Device-Id 请求头（设备级 UUID；留空时首启生成随机 UUID 并写回本配置）
    #[serde(default)]
    pub client_device_id: String,
    /// X-Device-Model 请求头（真实 Web 客户端发空串）
    #[serde(default)]
    pub client_device_model: String,
    /// X-Client-Timezone-Offset 请求头（真实 Web 客户端 UTC+8 发 28800）
    #[serde(default = "default_client_timezone_offset")]
    pub client_timezone_offset: String,
    /// 登录 payload 的 os 字段（与 UA 身份保持一致：web / android）
    #[serde(default = "default_client_os")]
    pub client_os: String,
    /// 传输层拟态档位（默认 `okhttp4_12`）
    ///
    /// - `okhttp4_12`：原生安卓 App 指纹，与默认 UA / `client_platform = android`
    ///   自洽（TLS/HTTP2 = OkHttp，默认头不含 `sec-ch-ua*` / `sec-fetch-*`）
    /// - `chrome136`：桌面 Chrome 指纹，仅当把 UA / `client_platform` / `client_os`
    ///   都切成 web 时才自洽
    #[serde(default = "default_emulation")]
    pub emulation: String,
    /// 是否在 completion 请求上回传 `x-hif-leim` 风控令牌（默认 true）
    ///
    /// 真实 Web/App 客户端会轮询 `hif-leim.deepseek.com` 取令牌，并在 SSE
    /// （completion）请求上以 `x-hif-leim` 头回传；缺少该头时上游可直接判定
    /// 请求并非来自官方客户端。设为 `false` 仅用于对照实验。
    #[serde(default = "default_hif_enabled")]
    pub hif_enabled: bool,
    /// 启动时是否用一条 completion 做健康检查（默认 `true`）
    ///
    /// 真实客户端启动只做「登录 → check_device → 拉会话列表」，不发消息。
    /// 设为 `false` 可对齐该序列（账号可用性仍由登录 + 禁言早检保证），
    /// 用于风控对照实验。
    #[serde(default = "default_startup_health_check")]
    pub startup_health_check: bool,
    /// 会话策略：`per_request`（默认，每轮新建并在结束时删除）或 `reuse`
    ///
    /// 真实客户端一个会话长期复用、几乎不删；`reuse` 会把会话保留在账号上复用，
    /// 空闲超过 `session_idle_secs` 才删除。**该模式尚未用真实账号验证过上游语义**
    /// （复用会话时仍以 `parent_message_id = null` 发言），仅用于对照实验。
    #[serde(default = "default_session_policy")]
    pub session_policy: String,
    /// 复用模式下会话的空闲回收秒数（`session_reuse = true` 时生效，0 = 只在进程退出时删除）
    #[serde(default = "default_session_idle_secs")]
    pub session_idle_secs: u64,
    /// 定义支持的模型类型列表，每种类型会自动映射为 OpenAI 的 model_id：deepseek-<type>
    #[serde(default = "default_model_types")]
    pub model_types: Vec<String>,
    /// 各模型类型的输入 token 限制（与 model_types 按索引一一对应）
    #[serde(default = "default_max_input_tokens")]
    pub max_input_tokens: Vec<u32>,
    /// 各模型类型的输出 token 限制（与 model_types 按索引一一对应）
    #[serde(default = "default_max_output_tokens")]
    pub max_output_tokens: Vec<u32>,
    /// 各模型类型的单次输入字符数限制（与 model_types 按索引一一对应）
    #[serde(default = "default_input_character_limits")]
    pub input_character_limits: Vec<u32>,
    /// 模型别名：按 index 对齐 model_types，默认无别名
    #[serde(default)]
    pub model_aliases: Vec<String>,
    /// 工具调用标签配置（自定义回退标签）
    #[serde(default)]
    pub tool_call: ToolCallTagConfig,
    /// 每账号每小时请求上限（0 = 不限制）
    ///
    /// 实测同一账号累计约 215 次请求后会被上游禁言，且禁言是**延迟判定**的。
    /// 该配额在账号维度做滑动窗口限流：达到上限的账号在本小时内不再被分配，
    /// 由池中其他账号承接；所有账号都超限时返回 429（而非继续硬打上游）。
    #[serde(default = "default_hourly_request_quota")]
    pub hourly_request_quota: u64,
    /// 未显式传入 `web_search_options` 时是否默认开启搜索模式（默认 true）
    ///
    /// `true` 保持历史行为（始终搜索）；设为 `false` 则严格遵循 OpenAI 语义
    /// （未传即关闭），可减少 DeepSeek 侧的系统提示词注入。
    #[serde(default = "default_search_enabled")]
    pub default_search_enabled: bool,
    /// Responses API `previous_response_id` 缓存条数上限（进程内，默认 256）
    #[serde(default = "default_responses_store_capacity")]
    pub responses_store_capacity: usize,
    /// Responses API 上下文缓存存活秒数（默认 3600）
    #[serde(default = "default_responses_store_ttl_secs")]
    pub responses_store_ttl_secs: u64,
}

impl DsCoreSection {
    /// 生成 OpenAI 模型注册表映射
    #[must_use]
    /// 构建 model_id → model_type 的映射表
    ///
    /// 每个 model_type 会注册三种写法，便于客户端直接用裸名（例如 Claude Code
    /// 里把 `model` 设成 `default`，见 issue #99）：
    /// - `deepseek-{ty}`（标准 ID）
    /// - `{ty}`（裸 model_type 名）
    /// - `model_aliases[i]`（用户自定义别名，可选）
    pub fn model_registry(&self) -> std::collections::HashMap<String, String> {
        let mut map = std::collections::HashMap::new();
        for (i, ty) in self.model_types.iter().enumerate() {
            map.insert(format!("deepseek-{ty}").to_lowercase(), ty.clone());
            // 裸名：`default` / `expert` / `vision` 直接可用
            map.entry(ty.to_lowercase()).or_insert_with(|| ty.clone());
            if let Some(alias) = self.model_aliases.get(i) {
                let alias = alias.trim().to_lowercase();
                if !alias.is_empty() {
                    map.insert(alias, ty.clone());
                }
            }
        }
        map
    }
}

/// Admin 配置
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct AdminConfig {
    /// bcrypt 哈希后的密码
    #[serde(default)]
    pub password_hash: String,
    /// JWT 签名密钥（hex 编码的 32 字节随机值）
    #[serde(default)]
    pub jwt_secret: String,
    /// 最近一次 JWT 签发时间（用于吊销旧 token）
    #[serde(default)]
    pub jwt_issued_at: u64,
    /// 修改密码：旧密码明文（仅 PUT 接收，不落地 config.toml）
    #[serde(default, skip_serializing)]
    pub old_password: String,
    /// 修改密码：新密码明文（仅 PUT 接收，不落地 config.toml）
    #[serde(default, skip_serializing)]
    pub new_password: String,
}

/// API Key 条目
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ApiKeyEntry {
    pub key: String,
    pub description: String,
}

/// 单个账号配置
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Account {
    /// 邮箱（与 mobile 二选一）
    pub email: String,
    /// 手机号（与 email 二选一）
    pub mobile: String,
    /// 区号（与 mobile 配合使用，如 "+86"）
    pub area_code: String,
    /// 密码
    pub password: String,
    /// 浏览器设备指纹 ID（可选，规避登录风控用，见 config.example.toml 说明）
    #[serde(default)]
    pub device_id: String,
}

/// 代理配置
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ProxyConfig {
    /// 代理 URL，如 http://127.0.0.1:7890 或 socks5://127.0.0.1:7891
    pub url: Option<String>,
}

/// 工具调用标签配置
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ToolCallTagConfig {
    /// 额外开始标签（内置 `<|tool▁calls▁begin|>` + 模糊匹配，此处只加格式完全不同的变体）
    #[serde(default = "default_tool_call_starts")]
    pub extra_starts: Vec<String>,
    /// 额外结束标签（内置 `<|tool▁calls▁end|>` + 模糊匹配，此处只加格式完全不同的变体）
    #[serde(default = "default_tool_call_ends")]
    pub extra_ends: Vec<String>,
}

impl Default for ToolCallTagConfig {
    fn default() -> Self {
        Self {
            extra_starts: default_tool_call_starts(),
            extra_ends: default_tool_call_ends(),
        }
    }
}

// ── 默认值函数 ──────────────────────────────────────────────────────────

fn default_tool_call_starts() -> Vec<String> {
    vec![
        "<|tool_call_begin|>".into(),
        "<tool_calls>".into(),
        "<tool_call>".into(),
    ]
}

fn default_tool_call_ends() -> Vec<String> {
    vec![
        "<|tool_call_end|>".into(),
        "</tool_calls>".into(),
        "</tool_call>".into(),
    ]
}

/// 默认只启用 default 模型。
///
/// 上游 `/api/v0/client/settings` 的 `model_configs` 明确显示：
/// `default` 为 `enabled: true, switchable: true`，而 `expert` 与 `vision`
/// 均为 `enabled: false, switchable: false`（网页端已不再提供切换入口）。
/// 因此默认不再暴露这两个模型，避免用户请求必然失败。
/// 需要时仍可在 `config.toml` 中显式配置 `model_types = ["default", "expert", "vision"]`。
fn default_model_types() -> Vec<String> {
    vec!["default".to_string()]
}

fn default_max_input_tokens() -> Vec<u32> {
    vec![1_048_576]
}

fn default_max_output_tokens() -> Vec<u32> {
    vec![384_000]
}

/// 上游对全部 model_type 均返回 `input_character_limit: 2621440`，
/// 历史上 expert 的 163840 已过期（上游现已放开）。
fn default_input_character_limits() -> Vec<u32> {
    vec![2_621_440]
}

/// 每账号每小时请求上限默认值
///
/// 取 60：远低于实测触发禁言的 ~215 次/小时量级，同时单账号仍能支撑
/// 常规交互式使用（平均每分钟 1 次）。需要更高吞吐时请增加账号数量，
/// 而不是抬高这个值 —— 这正是本配额想传达的约束。
const fn default_hourly_request_quota() -> u64 {
    60
}

/// 未传 `web_search_options` 时默认是否开启搜索模式。
///
/// 默认 `false`（对齐浏览器）：官方客户端只有用户**显式打开**联网搜索才会搜索。
/// 历史上本代理默认 `true`（无条件搜索），那会产生真实用户不会有的请求形态；
/// 需要旧行为可在配置里显式设为 `true`。
const fn default_search_enabled() -> bool {
    // 对齐浏览器：官方客户端只有用户显式打开联网搜索才会搜索，
    // 默认不搜。客户端仍可用 `web_search_options` 显式开启。
    false
}

/// Responses API 上下文缓存条数上限
///
/// 每条缓存只保存一轮的 output 数组（通常几 KB），256 条约占用几 MB 内存。
const fn default_responses_store_capacity() -> usize {
    256
}

/// Responses API 上下文缓存存活时间
const fn default_responses_store_ttl_secs() -> u64 {
    3600
}

fn default_api_base() -> String {
    "https://chat.deepseek.com/api/v0".to_string()
}

fn default_wasm_url() -> String {
    "https://fe-static.deepseek.com/chat/static/sha3_wasm_bg.7b9ca65ddd.wasm".to_string()
}

/// 默认 UA：桌面 Chrome（与默认 `client_platform = web` / `emulation = chrome136` 自洽）
///
/// 依据 2026-10-07 的真实浏览器抓包：官方 Web 客户端就是一套 Chrome 身份。
fn default_user_agent() -> String {
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36".to_string()
}

fn default_client_version() -> String {
    "2.5.0".to_string()
}

fn default_client_platform() -> String {
    // 官方 Web 客户端发 `web`（抓包：x-client-platform: web）
    "web".to_string()
}

fn default_client_locale() -> String {
    "zh_CN".to_string()
}

fn default_client_bundle_id() -> String {
    "com.deepseek.chat".to_string()
}

fn default_client_timezone_offset() -> String {
    "28800".to_string()
}

fn default_client_os() -> String {
    // 登录 payload 的 os 字段；官方 Web 客户端为 `web`
    "web".to_string()
}

const fn default_hif_enabled() -> bool {
    true
}

/// 启动健康检查开关
///
/// 默认 `false`：**对齐真实客户端**的启动序列 —— 官方客户端启动只做
/// 「登录 → check_device → 拉会话列表」，不发任何消息；而我们的历史上会在初始化时
/// 发一条 completion（"启动即发消息"是真实用户不会有的行为）。
/// 账号可用性仍由登录成功 + 禁言早检（biz_code 5）保证；需要旧行为可显式设为 `true`。
const fn default_startup_health_check() -> bool {
    false
}

/// 会话策略：默认 `reuse`（对齐真实客户端）
///
/// 官方客户端一个会话长期复用、几乎不删；而「每个请求建会话 → 发一条 → 立刻删」
/// 是真实用户不会有的模式。需要旧行为可显式设为 `per_request`。
fn default_session_policy() -> String {
    "reuse".to_string()
}

/// 复用模式下会话空闲回收时间（默认 900s = 15 分钟）
const fn default_session_idle_secs() -> u64 {
    900
}

fn default_emulation() -> String {
    // 默认对齐浏览器：TLS/HTTP2 指纹 + 默认头都用 chrome136
    "chrome136".to_string()
}

/// HTTP 服务器配置（必填）
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ServerConfig {
    /// 监听地址
    pub host: String,
    /// 监听端口
    pub port: u16,
    /// CORS 允许的 Origin 列表，默认 `["http://localhost:22217"]`
    #[serde(default = "default_cors_origins")]
    pub cors_origins: Vec<String>,
}

fn default_cors_origins() -> Vec<String> {
    vec!["http://localhost:22217".to_string()]
}

// ── Config 实现 ─────────────────────────────────────────────────────────

impl Config {
    /// 从指定路径加载配置
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self, ConfigError> {
        let content = std::fs::read_to_string(path)?;
        let mut config: Self = toml::de::from_str(&content)?;
        config.dedup_accounts();
        config.validate()?;
        Ok(config)
    }

    /// 按 email（优先）或 mobile 去重，保留首次出现的账号
    fn dedup_accounts(&mut self) {
        let mut seen = std::collections::HashSet::new();
        self.ds_core.accounts.retain(|a| {
            let key = if a.email.is_empty() {
                a.mobile.clone()
            } else {
                a.email.clone()
            };
            seen.insert(key)
        });
    }

    /// 解析命令行参数并加载配置
    pub fn load_with_args(
        args: impl Iterator<Item = String>,
    ) -> Result<(Self, PathBuf), ConfigError> {
        let mut explicit_c = false;
        let mut config_path = None;
        let mut iter = args.skip(1);

        while let Some(arg) = iter.next() {
            if arg == "-c" {
                explicit_c = true;
                if let Some(path) = iter.next() {
                    config_path = Some(path);
                } else {
                    return Err(ConfigError::Cli("-c 参数需要指定路径".to_string()));
                }
            }
        }

        let path: PathBuf = config_path
            .map(PathBuf::from)
            .or_else(|| std::env::var("DS_CONFIG_PATH").ok().map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("config.toml"));

        if !path.exists() {
            if explicit_c {
                return Err(ConfigError::Io(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("指定配置文件不存在: {}", path.display()),
                )));
            }
            let default = Config {
                ds_core: DsCoreSection {
                    accounts: Vec::new(),
                    ..Default::default()
                },
                server: ServerConfig {
                    host: "127.0.0.1".into(),
                    port: 22217,
                    cors_origins: default_cors_origins(),
                },
                proxy: ProxyConfig::default(),
                admin: AdminConfig::default(),
                api_keys: Vec::new(),
            };
            if let Some(parent) = path.parent() {
                let parent_str = parent.as_os_str();
                if !parent_str.is_empty() {
                    std::fs::create_dir_all(parent)?;
                }
            }
            default.save(&path)?;
            log::info!(target: "config", "created default config file: {}", path.display());
            return Ok((default, path));
        }

        let config = Self::load(&path)?;

        Ok((config, path))
    }

    /// 验证配置有效性
    pub(crate) fn validate(&self) -> Result<(), ConfigError> {
        if self.ds_core.model_types.is_empty() {
            return Err(ConfigError::Validation("model_types 不能为空".to_string()));
        }
        let n = self.ds_core.model_types.len();
        if self.ds_core.max_input_tokens.len() != n {
            return Err(ConfigError::Validation(format!(
                "max_input_tokens 长度({})必须与 model_types 长度({})一致",
                self.ds_core.max_input_tokens.len(),
                n
            )));
        }
        if self.ds_core.max_output_tokens.len() != n {
            return Err(ConfigError::Validation(format!(
                "max_output_tokens 长度({})必须与 model_types 长度({})一致",
                self.ds_core.max_output_tokens.len(),
                n
            )));
        }
        if self.ds_core.input_character_limits.len() != n {
            return Err(ConfigError::Validation(format!(
                "input_character_limits 长度({})必须与 model_types 长度({})一致",
                self.ds_core.input_character_limits.len(),
                n
            )));
        }
        let mut seen_keys = std::collections::HashSet::new();
        for k in &self.api_keys {
            if !seen_keys.insert(&k.key) {
                // 按字符截断，避免非 ASCII key 在字节切片处 panic
                let prefix: String = k.key.chars().take(12).collect();
                return Err(ConfigError::Validation(format!(
                    "API key 重复: {prefix}..."
                )));
            }
        }
        Ok(())
    }

    /// 原子保存配置到文件
    pub fn save(&self, path: impl AsRef<Path>) -> Result<(), ConfigError> {
        let toml_str = toml::to_string_pretty(self).map_err(ConfigError::TomlSerialization)?;
        let tmp = path.as_ref().with_extension("toml.tmp");
        std::fs::write(&tmp, &toml_str)?;
        std::fs::rename(&tmp, path.as_ref())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o600);
            std::fs::set_permissions(path.as_ref(), perms)?;
        }
        Ok(())
    }
}

impl Default for DsCoreSection {
    fn default() -> Self {
        Self {
            accounts: Vec::new(),
            api_base: default_api_base(),
            wasm_url: default_wasm_url(),
            user_agent: default_user_agent(),
            client_version: default_client_version(),
            client_platform: default_client_platform(),
            client_locale: default_client_locale(),
            client_bundle_id: default_client_bundle_id(),
            client_device_id: String::new(),
            client_device_model: String::new(),
            client_timezone_offset: default_client_timezone_offset(),
            client_os: default_client_os(),
            emulation: default_emulation(),
            hif_enabled: default_hif_enabled(),
            startup_health_check: default_startup_health_check(),
            session_policy: default_session_policy(),
            session_idle_secs: default_session_idle_secs(),
            model_types: default_model_types(),
            max_input_tokens: default_max_input_tokens(),
            max_output_tokens: default_max_output_tokens(),
            input_character_limits: default_input_character_limits(),
            model_aliases: Vec::new(),
            tool_call: ToolCallTagConfig::default(),
            hourly_request_quota: default_hourly_request_quota(),
            default_search_enabled: default_search_enabled(),
            responses_store_capacity: default_responses_store_capacity(),
            responses_store_ttl_secs: default_responses_store_ttl_secs(),
        }
    }
}

/// 配置加载错误类型
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("IO 错误: {0}")]
    Io(#[from] std::io::Error),
    #[error("TOML 解析错误: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("配置验证错误: {0}")]
    Validation(String),
    #[error("命令行参数错误: {0}")]
    Cli(String),
    #[error("TOML 序列化错误: {0}")]
    TomlSerialization(#[from] toml::ser::Error),
}
