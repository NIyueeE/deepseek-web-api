//! OpenAI 协议适配层 —— OpenAI JSON 与 ds_core 内部格式的双向转换
//!
//! 本模块负责将 OpenAI 兼容的 HTTP 请求转换为 ds_core 内部格式，
//! 并将 ds_core 的响应转换为 OpenAI 兼容的 JSON 格式。
//!
//! 对外暴露最小接口：OpenAIAdapter, OpenAIAdapterError

use std::pin::Pin;
use std::sync::Arc;

use bytes::Bytes;
use futures::Stream;

use ds_core::{AccountConfig, CoreError, DsCore, DsCoreConfig};
use std::collections::HashMap;

mod models;
pub(crate) mod request;
pub(crate) mod response;
pub(crate) mod types;

pub use types::{ChatCompletionsRequest, ChatCompletionsResponse, ChatCompletionsResponseChunk};

/// 流式响应类型（SSE 字节流）
pub type StreamResponse = Pin<Box<dyn Stream<Item = Result<Bytes, OpenAIAdapterError>> + Send>>;

/// 流式响应结构体流
pub type ChunkStream =
    Pin<Box<dyn Stream<Item = Result<ChatCompletionsResponseChunk, OpenAIAdapterError>> + Send>>;

/// Chat Completions 统一输出
pub enum ChatOutput {
    Stream(ChunkStream),
    Json(ChatCompletionsResponse),
}

/// adapter 层通用结果包装：携带请求结果和账号标识
pub struct ChatResult<T> {
    pub data: T,
    pub account_id: String,
    pub prompt_tokens: u32,
}

/// OpenAI 适配器
pub struct OpenAIAdapter {
    ds_core: Arc<DsCore>,
    model_types: tokio::sync::RwLock<Vec<String>>,
    model_registry: tokio::sync::RwLock<HashMap<String, String>>,
    model_aliases: tokio::sync::RwLock<Vec<String>>,
    max_input_tokens: tokio::sync::RwLock<Vec<u32>>,
    max_output_tokens: tokio::sync::RwLock<Vec<u32>>,
    tag_config: tokio::sync::RwLock<Arc<response::TagConfig>>,
    /// 未显式传 `web_search_options` 时是否默认开启搜索模式
    default_search_enabled: std::sync::atomic::AtomicBool,
    /// 缓存的 tiktoken BPE 编码器（避免每次请求重建）
    bpe: Option<Arc<tiktoken_rs::CoreBPE>>,
}

impl OpenAIAdapter {
    /// 创建适配器实例
    pub async fn new(config: &crate::config::Config) -> Result<Self, OpenAIAdapterError> {
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
            emulation: config.ds_core.emulation.clone(),
            hif_enabled: config.ds_core.hif_enabled,
            proxy_url: config.proxy.url.clone(),
            model_types: config.ds_core.model_types.clone(),
            input_character_limits: config.ds_core.input_character_limits.clone(),
            hourly_request_quota: config.ds_core.hourly_request_quota,
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
        let ds_core = Arc::new(DsCore::new(&core_cfg, accounts).await?);
        let model_registry = config.ds_core.model_registry();
        // 预初始化 tiktoken BPE（避免每次请求重建词表）
        let bpe = tiktoken_rs::cl100k_base().ok().map(Arc::new);

        Ok(Self {
            ds_core,
            model_types: tokio::sync::RwLock::new(config.ds_core.model_types.clone()),
            model_registry: tokio::sync::RwLock::new(model_registry),
            model_aliases: tokio::sync::RwLock::new(config.ds_core.model_aliases.clone()),
            max_input_tokens: tokio::sync::RwLock::new(config.ds_core.max_input_tokens.clone()),
            max_output_tokens: tokio::sync::RwLock::new(config.ds_core.max_output_tokens.clone()),
            tag_config: tokio::sync::RwLock::new(Arc::new(response::TagConfig::from_config(
                &config.ds_core.tool_call,
            ))),
            default_search_enabled: std::sync::atomic::AtomicBool::new(
                config.ds_core.default_search_enabled,
            ),
            bpe,
        })
    }

    /// POST /v1/chat/completions（统一入口）
    ///
    /// 内部校验参数、构建 ChatML prompt、按 stream 标记分流：
    /// - stream=true  → 返回 SSE 字节流
    /// - stream=false → 将 SSE 流聚合为单个 JSON 对象后返回
    pub async fn chat_completions(
        &self,
        mut req: ChatCompletionsRequest,
        request_id: &str,
    ) -> Result<ChatResult<ChatOutput>, OpenAIAdapterError> {
        log::debug!(target: "adapter", "req={} adapter start: model={}, stream={}", request_id, req.model, req.stream);
        use crate::openai_adapter::types::{
            FunctionCallOption, NamedFunction, NamedToolChoice, Tool, ToolChoice,
        };

        // 兼容旧版 functions / function_call → tools / tool_choice
        if req.tools.as_ref().map(|t| t.is_empty()).unwrap_or(true)
            && let Some(functions) = req.functions.clone()
            && !functions.is_empty()
        {
            req.tools = Some(
                functions
                    .into_iter()
                    .map(|f| Tool {
                        ty: "function".to_string(),
                        function: Some(f),
                        custom: None,
                    })
                    .collect(),
            );
        }
        if req.tool_choice.is_none()
            && let Some(fc) = req.function_call.clone()
        {
            req.tool_choice = Some(match fc {
                FunctionCallOption::Mode(mode) => ToolChoice::Mode(mode),
                FunctionCallOption::Named(named) => ToolChoice::Named(NamedToolChoice {
                    ty: "function".to_string(),
                    function: NamedFunction { name: named.name },
                }),
            });
        }

        let norm = request::normalize::apply(&req).map_err(OpenAIAdapterError::BadRequest)?;
        let tool_ctx = request::tools::extract(&req).map_err(OpenAIAdapterError::BadRequest)?;
        let prompt = request::prompt::build(&req, &tool_ctx);
        let registry = self.model_registry.read().await;
        let model_res = request::resolver::resolve(
            &registry,
            &req.model,
            req.reasoning_effort.as_deref(),
            req.web_search_options.as_ref(),
            self.default_search(),
        )
        .map_err(OpenAIAdapterError::BadRequest)?;

        let prompt_tokens = self
            .bpe
            .as_ref()
            .map(|bpe| {
                u32::try_from(bpe.encode_with_special_tokens(&prompt).len())
                    .expect("token count exceeds u32::MAX")
            })
            .unwrap_or(0);

        let file_result = request::files::extract(&req);
        let chat_req = ds_core::ChatRequest {
            prompt,
            thinking_enabled: model_res.thinking_enabled,
            search_enabled: model_res.search_enabled || file_result.has_http_urls,
            model_type: model_res.model_type,
            files: file_result.files,
        };

        let chat_resp = self.try_chat(chat_req, request_id).await?;
        let (account_id, event_stream) = Self::take_meta(chat_resp.stream).await.map_err(|e| {
            log::error!(target: "adapter", "req={} failed to read Meta event: {}", request_id, e);
            OpenAIAdapterError::Internal("取 Meta 事件失败".into())
        })?;

        // 为修复模型准备工具定义信息
        let tool_defs = req.tools.as_ref().map(|tools| {
            tools
                .iter()
                .filter_map(|t| t.function.as_ref())
                .map(|f| {
                    format!(
                        "- {}: {}",
                        f.name,
                        serde_json::to_string(&f.parameters).unwrap_or_default()
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        });

        if req.stream {
            let repair_fn = self.create_repair_fn(request_id, tool_defs.clone()).await;
            let s = response::stream(
                event_stream,
                req.model,
                response::StreamCfg {
                    include_usage: norm.include_usage,
                    include_obfuscation: norm.include_obfuscation,
                    stop: norm.stop,
                    prompt_tokens,
                    repair_fn: Some(repair_fn),
                    tag_config: self.tag_config.read().await.clone(),
                },
            );
            Ok(ChatResult {
                data: ChatOutput::Stream(s),
                account_id,
                prompt_tokens,
            })
        } else {
            let repair_fn = self.create_repair_fn(request_id, tool_defs).await;
            let json = response::aggregate(
                event_stream,
                req.model,
                response::StreamCfg {
                    include_usage: true,
                    include_obfuscation: false,
                    stop: norm.stop,
                    prompt_tokens,
                    repair_fn: Some(repair_fn),
                    tag_config: self.tag_config.read().await.clone(),
                },
            )
            .await?;
            Ok(ChatResult {
                data: ChatOutput::Json(json),
                account_id,
                prompt_tokens,
            })
        }
    }

    /// 内部辅助：对 `Overloaded` 进行退避重试（v0_chat 内部已做换号重试，此处为号池级兜底）
    pub(crate) async fn try_chat(
        &self,
        req: ds_core::ChatRequest,
        request_id: &str,
    ) -> Result<ds_core::ChatResponse, CoreError> {
        const MAX_RETRIES: usize = 2;
        const BASE_DELAY_MS: u64 = 2000;

        for attempt in 0..MAX_RETRIES {
            match self.ds_core.v0_chat(req.clone(), request_id).await {
                Ok(resp) => {
                    if attempt > 0 {
                        log::info!(target: "adapter", "req={} retry #{} succeeded", request_id, attempt);
                    }
                    return Ok(resp);
                }
                Err(CoreError::Overloaded) if attempt + 1 < MAX_RETRIES => {
                    let delay = BASE_DELAY_MS * (1 << attempt);
                    log::warn!(target: "adapter", "req={} overloaded, retry #{} in {}ms", request_id, attempt + 1, delay);
                    tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                }
                Err(e) => return Err(e),
            }
        }
        log::warn!(target: "adapter", "req={} all {} retries failed, giving up", request_id, MAX_RETRIES);
        Err(CoreError::Overloaded)
    }

    /// 读取「默认是否开启搜索模式」开关
    fn default_search(&self) -> bool {
        self.default_search_enabled
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// GET /v1/models
    pub async fn list_models(&self) -> types::OpenAIModelList {
        let model_types = self.model_types.read().await;
        let max_input = self.max_input_tokens.read().await;
        let max_output = self.max_output_tokens.read().await;
        let aliases = self.model_aliases.read().await;
        models::list(&model_types, &max_input, &max_output, &aliases)
    }

    /// GET /v1/models/{model_id}
    pub async fn get_model(&self, model_id: &str) -> Option<types::OpenAIModel> {
        let model_types = self.model_types.read().await;
        let max_input = self.max_input_tokens.read().await;
        let max_output = self.max_output_tokens.read().await;
        let aliases = self.model_aliases.read().await;
        models::get(&model_types, &max_input, &max_output, &aliases, model_id)
    }

    /// 原始 DeepSeek SSE 流（不经 OpenAI 协议转换）
    ///
    /// 用于流分析：对比原始响应与 OpenAI 转换后的差异，定位转换 bug
    pub async fn raw_chat_completions_stream(
        &self,
        body: &[u8],
        request_id: &str,
    ) -> Result<ChatResult<StreamResponse>, OpenAIAdapterError> {
        let chat_req: ChatCompletionsRequest = serde_json::from_slice(body)
            .map_err(|e| OpenAIAdapterError::BadRequest(format!("bad request: {}", e)))?;
        let registry = self.model_registry.read().await;
        let model_res = request::resolver::resolve(
            &registry,
            &chat_req.model,
            chat_req.reasoning_effort.as_deref(),
            chat_req.web_search_options.as_ref(),
            self.default_search(),
        )
        .map_err(OpenAIAdapterError::BadRequest)?;
        let ds_req = ds_core::ChatRequest {
            prompt: request::prompt::build(
                &chat_req,
                &request::tools::extract(&chat_req).map_err(OpenAIAdapterError::BadRequest)?,
            ),
            thinking_enabled: model_res.thinking_enabled,
            search_enabled: model_res.search_enabled,
            model_type: model_res.model_type,
            files: vec![],
        };
        let chat_resp = self.try_chat(ds_req, request_id).await?;
        let (account_id, event_stream) = Self::take_meta(chat_resp.stream).await?;

        // 将 StreamEvent 序列化为 JSON 行供调试
        use futures::StreamExt;
        let data: StreamResponse = Box::pin(event_stream.map(|r| {
            r.map(|evt| {
                let line = format!("{:?}\n", evt);
                Bytes::from(line.into_bytes())
            })
            .map_err(OpenAIAdapterError::from)
        }));
        Ok(ChatResult {
            data,
            account_id,
            prompt_tokens: 0,
        })
    }

    /// 获取 ds_core 账号池状态
    pub fn account_statuses(&self) -> Vec<ds_core::AccountStatus> {
        self.ds_core.account_statuses()
    }

    /// 获取 ds_core 账号池详细状态（含滑动窗口统计、请求间隔等）
    pub fn account_statuses_detailed(&self) -> Vec<ds_core::AccountStatus> {
        self.ds_core.account_statuses_detailed()
    }

    /// 拉取账号的会话列表（`updated_at` 为分页游标，None = 第一页）
    pub async fn fetch_sessions(
        &self,
        updated_at: Option<f64>,
    ) -> Result<ds_core::FetchSessionsData, OpenAIAdapterError> {
        self.ds_core
            .fetch_sessions(updated_at)
            .await
            .map_err(OpenAIAdapterError::from)
    }

    /// 动态添加账号
    pub async fn add_account(
        &self,
        creds: &crate::config::Account,
    ) -> Result<String, ds_core::PoolError> {
        let ac = AccountConfig {
            email: creds.email.clone(),
            mobile: creds.mobile.clone(),
            area_code: creds.area_code.clone(),
            password: creds.password.clone(),
            device_id: creds.device_id.clone(),
        };
        self.ds_core.add_account(&ac).await
    }

    /// 动态移除账号
    pub async fn remove_account(
        &self,
        email_or_mobile: &str,
    ) -> Result<String, ds_core::PoolError> {
        self.ds_core.remove_account(email_or_mobile).await
    }

    /// 标记账号为 Error 状态
    pub fn mark_error(&self, email_or_mobile: &str) {
        self.ds_core.mark_error(email_or_mobile);
    }

    /// 手动重新登录指定账号
    pub async fn re_login_single(&self, email_or_mobile: &str) -> Result<(), String> {
        self.ds_core.re_login_single(email_or_mobile).await
    }
}

impl OpenAIAdapter {
    /// 批量同步账号：对比当前账号池与目标配置，增删差异账号
    pub(crate) async fn sync_accounts(&self, new_accounts: &[crate::config::Account]) {
        let old_statuses = self.account_statuses();
        let old_ids: Vec<String> = old_statuses
            .iter()
            .map(|a| {
                if a.email.is_empty() {
                    a.mobile.clone()
                } else {
                    a.email.clone()
                }
            })
            .collect();

        let mut _added = 0usize;
        let mut _failed = 0usize;
        for acct in new_accounts {
            let id = if acct.email.is_empty() {
                &acct.mobile
            } else {
                &acct.email
            };
            if !old_ids.contains(id) {
                match self.add_account(acct).await {
                    Ok(_) => _added += 1,
                    Err(e) => {
                        log::warn!(target: "adapter", "failed to add account during sync {}: {}", id, e);
                        _failed += 1;
                    }
                }
            }
        }

        let mut _removed = 0usize;
        let new_ids: Vec<&str> = new_accounts
            .iter()
            .map(|a| {
                if a.email.is_empty() {
                    a.mobile.as_str()
                } else {
                    a.email.as_str()
                }
            })
            .collect();
        for old_id in &old_ids {
            if !new_ids.contains(&old_id.as_str()) && !old_id.is_empty() {
                match self.remove_account(old_id).await {
                    Ok(_) => _removed += 1,
                    Err(e) => {
                        log::warn!(target: "adapter", "failed to remove account during sync {}: {}", old_id, e);
                    }
                }
            }
        }
    }

    /// 从流中提取首个 Meta 事件的 account_id，同时保留 Meta 在流中
    async fn take_meta(
        stream: Pin<Box<dyn Stream<Item = Result<ds_core::StreamEvent, CoreError>> + Send>>,
    ) -> Result<
        (
            String,
            Pin<Box<dyn Stream<Item = Result<ds_core::StreamEvent, CoreError>> + Send>>,
        ),
        CoreError,
    > {
        use futures::StreamExt;
        let mut stream = stream;
        match stream.next().await {
            Some(Ok(ds_core::StreamEvent::Meta { account_id })) => {
                let full =
                    futures::stream::once(futures::future::ready(Ok(ds_core::StreamEvent::Meta {
                        account_id: account_id.clone(),
                    })))
                    .chain(stream);
                Ok((account_id, Box::pin(full)))
            }
            Some(Ok(other)) => {
                log::warn!(target: "adapter", "expected Meta as first event, got: {other:?}");
                let rest = futures::stream::once(futures::future::ready(Ok(other))).chain(stream);
                Ok((String::new(), Box::pin(rest)))
            }
            Some(Err(e)) => Err(e),
            None => Err(CoreError::Stream("empty stream".into())),
        }
    }

    /// 优雅关闭
    pub async fn shutdown(&self) {
        self.ds_core.shutdown().await;
    }

    pub async fn reload_config(&self, new_config: &crate::config::Config) -> Result<(), CoreError> {
        // Sync accounts
        self.sync_accounts(&new_config.ds_core.accounts).await;
        // Rebuild model registry
        let registry = new_config.ds_core.model_registry();
        *self.model_registry.write().await = registry;
        *self.model_types.write().await = new_config.ds_core.model_types.clone();
        *self.model_aliases.write().await = new_config.ds_core.model_aliases.clone();
        *self.max_input_tokens.write().await = new_config.ds_core.max_input_tokens.clone();
        *self.max_output_tokens.write().await = new_config.ds_core.max_output_tokens.clone();
        *self.tag_config.write().await = Arc::new(response::TagConfig::from_config(
            &new_config.ds_core.tool_call,
        ));
        self.default_search_enabled.store(
            new_config.ds_core.default_search_enabled,
            std::sync::atomic::Ordering::Relaxed,
        );
        // Rebuild DsClient if needed (deepseek/proxy changes)
        let core_cfg = DsCoreConfig {
            api_base: new_config.ds_core.api_base.clone(),
            wasm_url: new_config.ds_core.wasm_url.clone(),
            user_agent: new_config.ds_core.user_agent.clone(),
            client_version: new_config.ds_core.client_version.clone(),
            client_platform: new_config.ds_core.client_platform.clone(),
            client_locale: new_config.ds_core.client_locale.clone(),
            client_bundle_id: new_config.ds_core.client_bundle_id.clone(),
            client_device_id: new_config.ds_core.client_device_id.clone(),
            client_device_model: new_config.ds_core.client_device_model.clone(),
            client_timezone_offset: new_config.ds_core.client_timezone_offset.clone(),
            client_os: new_config.ds_core.client_os.clone(),
            emulation: new_config.ds_core.emulation.clone(),
            hif_enabled: new_config.ds_core.hif_enabled,
            proxy_url: new_config.proxy.url.clone(),
            model_types: new_config.ds_core.model_types.clone(),
            input_character_limits: new_config.ds_core.input_character_limits.clone(),
            hourly_request_quota: new_config.ds_core.hourly_request_quota,
        };
        self.ds_core.reload_config(&core_cfg).await
    }

    pub(crate) async fn create_repair_fn(
        &self,
        request_id: &str,
        tool_defs: Option<String>,
    ) -> response::RepairFn {
        use std::sync::atomic::{AtomicU16, Ordering};
        let core = self.ds_core.clone();
        let req_id = request_id.to_string();
        let seq = Arc::new(AtomicU16::new(0));
        let tag_config = self.tag_config.read().await.clone();
        let tools_info = tool_defs.unwrap_or_default();
        Arc::new(move |tool_text: String| {
            let core = core.clone();
            let req_id = req_id.clone();
            let seq = seq.clone();
            let tag_config = tag_config.clone();
            let tools_info = tools_info.clone();
            Box::pin(async move {
                use ds_core::ChatRequest;
                let n = seq.fetch_add(1, Ordering::Relaxed);
                let repair_req_id = format!("{}-repair-{}", req_id, n);
                let mut prompt = String::new();
                if !tools_info.is_empty() {
                    prompt.push_str(&format!("可用的工具定义：\n{}\n\n", tools_info));
                }
                prompt.push_str(&format!(
                    "请将以下代码块中的内容提取并转换为合法的工具调用 JSON 数组。\
                     \n每个元素必须包含 \"name\"（字符串）和 \"arguments\"（对象）字段。\
                     \n只输出 JSON 数组本身，不要加 code fence，不要其他文字解释。\
                     \n注意：字符串值中的引号和换行符必须用反斜杠转义（如 \\\" 和 \\n）。\
                     \n\n需要修复的内容：\n~~~\n{tool_text}\n~~~"
                ));
                let req = ChatRequest {
                    prompt,
                    thinking_enabled: false,
                    search_enabled: false,
                    model_type: "default".to_string(),
                    files: vec![],
                };
                log::debug!(
                    target: "adapter",
                    "{} 发起修复请求: len={}", repair_req_id, tool_text.len()
                );
                let resp = core
                    .v0_chat(req, &repair_req_id)
                    .await
                    .map_err(OpenAIAdapterError::from)?;
                response::execute_tool_repair(resp.stream, &tag_config).await
            })
        })
    }
}

/// 适配器错误类型
#[derive(Debug, thiserror::Error)]
pub enum OpenAIAdapterError {
    /// 请求格式错误
    #[error("bad request: {0}")]
    BadRequest(String),

    /// 服务过载，无可用的 ds_core 账号
    #[error("service overloaded")]
    Overloaded,

    /// 上游提供商错误（网络、业务错误等）
    #[error("provider error: {0}")]
    ProviderError(String),

    /// 内部错误（序列化、流转换等）
    #[error("internal error: {0}")]
    Internal(String),

    /// tool_calls 标记解析失败，携带 `{TOOL_CALL_START}...{TOOL_CALL_END}` 内的原始文本
    #[error("tool_calls repair needed: {0}")]
    ToolCallRepairNeeded(String),
}

impl From<CoreError> for OpenAIAdapterError {
    fn from(e: CoreError) -> Self {
        match e {
            CoreError::Overloaded => Self::Overloaded,
            CoreError::ProofOfWorkFailed(err) => {
                Self::Internal(format!("proof of work failed: {}", err))
            }
            CoreError::ProviderError(msg) => Self::ProviderError(msg),
            CoreError::Stream(msg) => Self::Internal(msg),
        }
    }
}

impl From<serde_json::Error> for OpenAIAdapterError {
    fn from(e: serde_json::Error) -> Self {
        Self::Internal(format!("json serialization failed: {}", e))
    }
}

impl OpenAIAdapterError {
    /// 返回对应 HTTP 状态码
    #[must_use]
    pub fn status_code(&self) -> u16 {
        match self {
            Self::BadRequest(_) => 400,
            Self::Overloaded => 429,
            Self::ProviderError(_) => 502,
            Self::Internal(_) | Self::ToolCallRepairNeeded(_) => 500,
        }
    }
}
