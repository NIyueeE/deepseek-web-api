//! 模型解析 —— 将 OpenAI model 字段映射为 ds_core 能力标志
//!
//! 通过外部注入的 registry 实现模型别名到 model_type 的动态映射。

use std::collections::HashMap;

use crate::openai_adapter::types::WebSearchOptions;

/// 模型解析结果
pub(crate) struct ModelResolution {
    /// ds_core 使用的 model_type
    pub model_type: String,
    pub thinking_enabled: bool,
    pub search_enabled: bool,
}

/// 根据 model_id 和扩展参数解析模型配置
///
/// # thinking
///
/// `reasoning_effort` 非 `"none"` 时启用；未提供时按 `"high"` 处理（默认开启）。
///
/// # search
///
/// 搜索模式会让 DeepSeek 后端注入更强的系统提示词。判定顺序：
///
/// 1. 显式提供 `web_search_options` → **开启**（客户端明确要求联网）
/// 2. 未提供 → 取 `default_search_enabled`（配置项，默认 `true`）
///
/// 第 2 条的默认值保持历史行为（始终开启），避免升级后静默改变推理结果；
/// 需要严格遵循 OpenAI 语义（未传即关闭）的部署可设置
/// `default_search_enabled = false`。
///
/// 注意：调用方还会在此基础上叠加「prompt 里含 HTTP URL 则强制开启」的规则。
pub(crate) fn resolve(
    registry: &HashMap<String, String>,
    model_id: &str,
    reasoning_effort: Option<&str>,
    web_search_options: Option<&WebSearchOptions>,
    default_search_enabled: bool,
) -> Result<ModelResolution, String> {
    let key = model_id.to_lowercase();
    let model_type = registry
        .get(&key)
        .cloned()
        .ok_or_else(|| format!("不支持的模型: {model_id}"))?;

    let reasoning_effort = reasoning_effort.unwrap_or("high");
    let thinking_enabled = reasoning_effort != "none";

    let search_enabled = web_search_options.is_some() || default_search_enabled;

    Ok(ModelResolution {
        model_type,
        thinking_enabled,
        search_enabled,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> HashMap<String, String> {
        let mut m = HashMap::new();
        m.insert("deepseek-default".to_string(), "default".to_string());
        m.insert("default".to_string(), "default".to_string());
        m
    }

    fn opts() -> WebSearchOptions {
        WebSearchOptions {
            search_context_size: Some("high".to_string()),
            user_location: None,
        }
    }

    #[test]
    fn unknown_model_is_rejected() {
        assert!(resolve(&registry(), "nope", None, None, true).is_err());
    }

    #[test]
    fn bare_and_prefixed_model_ids_resolve() {
        for id in ["deepseek-default", "DEFAULT", "default"] {
            let r = resolve(&registry(), id, None, None, true).unwrap();
            assert_eq!(r.model_type, "default");
        }
    }

    #[test]
    fn explicit_web_search_options_always_enables_search() {
        let r = resolve(&registry(), "default", None, Some(&opts()), false).unwrap();
        assert!(r.search_enabled, "客户端显式请求联网时必须开启");
    }

    #[test]
    fn search_follows_config_when_options_absent() {
        let on = resolve(&registry(), "default", None, None, true).unwrap();
        assert!(on.search_enabled, "默认配置应保持历史行为（开启）");

        let off = resolve(&registry(), "default", None, None, false).unwrap();
        assert!(
            !off.search_enabled,
            "配置为 false 时未传 web_search_options 应关闭"
        );
    }

    #[test]
    fn reasoning_defaults_on_and_can_be_disabled() {
        let default_on = resolve(&registry(), "default", None, None, true).unwrap();
        assert!(
            default_on.thinking_enabled,
            "未指定 reasoning_effort 时默认开启"
        );

        let off = resolve(&registry(), "default", Some("none"), None, true).unwrap();
        assert!(!off.thinking_enabled);

        for effort in ["low", "medium", "high"] {
            let r = resolve(&registry(), "default", Some(effort), None, true).unwrap();
            assert!(r.thinking_enabled, "{effort} 应开启 thinking");
        }
    }
}
