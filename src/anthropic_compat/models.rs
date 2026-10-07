//! Anthropic Models API 响应生成
//!
//! 基于 openai_adapter 的 ModelList，转换为 Anthropic /v1/models 响应格式。

use log::debug;

use serde::{Deserialize, Serialize};

use crate::openai_adapter::types::OpenAIModel;
use crate::openai_adapter::types::OpenAIModelList;

// ============================================================================
// Anthropic 协议类型
// ============================================================================

/// 模型能力信息
#[derive(Debug, Serialize, Deserialize)]
struct ModelCapabilities {
    thinking: ThinkingCapability,
    image_input: CapabilitySupport,
    pdf_input: CapabilitySupport,
    structured_outputs: CapabilitySupport,
}

/// Thinking 能力
#[derive(Debug, Serialize, Deserialize)]
struct ThinkingCapability {
    supported: bool,
    types: ThinkingTypes,
}

/// Thinking 类型支持
#[derive(Debug, Serialize, Deserialize)]
struct ThinkingTypes {
    enabled: CapabilitySupport,
    adaptive: CapabilitySupport,
}

/// 单项能力支持
#[derive(Debug, Serialize, Deserialize)]
struct CapabilitySupport {
    supported: bool,
}

/// 单个模型信息
#[derive(Debug, Serialize, Deserialize)]
pub struct AnthropicModel {
    id: String,
    #[serde(rename = "type")]
    ty: String,
    display_name: String,
    created_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_input_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    capabilities: ModelCapabilities,
}

/// 模型列表响应
#[derive(Debug, Serialize, Deserialize)]
pub struct AnthropicModelList {
    data: Vec<AnthropicModel>,
    has_more: bool,
    first_id: String,
    last_id: String,
}

// ============================================================================
// 响应生成
// ============================================================================

/// 根据 OpenAI ModelList 生成 Anthropic 格式响应
pub(crate) fn list(list: &OpenAIModelList) -> AnthropicModelList {
    debug!(target: "anthropic_compat::models", "building model list");

    let data: Vec<AnthropicModel> = list.data.iter().map(to_anthropic_model).collect();

    let first_id = data.first().map(|m| m.id.clone()).unwrap_or_default();
    let last_id = data.last().map(|m| m.id.clone()).unwrap_or_default();

    AnthropicModelList {
        data,
        has_more: false,
        first_id,
        last_id,
    }
}

/// 查询单个模型
pub(crate) fn get(list: &OpenAIModelList, model_id: &str) -> Option<AnthropicModel> {
    list.data
        .iter()
        .find(|m| m.id == model_id)
        .map(to_anthropic_model)
}

// ============================================================================
// 模型映射
// ============================================================================

fn to_anthropic_model(m: &OpenAIModel) -> AnthropicModel {
    let display_name = id_to_display_name(&m.id);

    AnthropicModel {
        id: m.id.clone(),
        ty: "model".to_string(),
        display_name,
        created_at: unix_to_rfc3339(m.created),
        max_input_tokens: m.max_input_tokens,
        max_tokens: m.max_output_tokens,
        capabilities: ModelCapabilities {
            thinking: ThinkingCapability {
                supported: true,
                types: ThinkingTypes {
                    enabled: CapabilitySupport { supported: true },
                    adaptive: CapabilitySupport { supported: true },
                },
            },
            image_input: CapabilitySupport { supported: true },
            pdf_input: CapabilitySupport { supported: true },
            structured_outputs: CapabilitySupport { supported: true },
        },
    }
}

/// 将 "deepseek-expert" 解析为 "DeepSeek Expert"
fn id_to_display_name(id: &str) -> String {
    id.split('-')
        .map(|word| {
            let mut chars = word.chars();
            chars.next().map_or_else(String::new, |first| {
                format!(
                    "{}{}",
                    first.to_uppercase().collect::<String>(),
                    chars.as_str().to_lowercase()
                )
            })
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// 将 Unix 时间戳（秒）转为 RFC 3339 格式字符串（UTC）
fn unix_to_rfc3339(secs: u64) -> String {
    let days = secs / 86_400;
    let rem_secs = secs % 86_400;

    let hour = (rem_secs / 3_600) as u32;
    let rem = rem_secs % 3_600;
    let minute = (rem / 60) as u32;
    let second = (rem % 60) as u32;

    let mut year = 1970u64;
    let mut remaining_days = days;

    loop {
        let year_len = if is_leap_year(year) { 366 } else { 365 };
        if remaining_days < year_len {
            break;
        }
        remaining_days -= year_len;
        year += 1;
    }

    let month_days = [
        31,
        if is_leap_year(year) { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];

    let mut month = 1u32;
    let mut day = u32::try_from(remaining_days + 1).unwrap_or(u32::MAX);
    for dim in &month_days {
        if day <= *dim {
            break;
        }
        day -= dim;
        month += 1;
    }

    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

const fn is_leap_year(y: u64) -> bool {
    y.is_multiple_of(4) && !y.is_multiple_of(100) || y.is_multiple_of(400)
}

// ============================================================================
// 测试
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_list() -> OpenAIModelList {
        OpenAIModelList {
            object: "list",
            data: vec![
                OpenAIModel {
                    id: "deepseek-default".to_string(),
                    object: "model",
                    created: 1_090_108_800,
                    owned_by: "x",
                    max_input_tokens: None,
                    max_output_tokens: None,
                    context_length: None,
                    context_window: None,
                    max_context_length: None,
                    max_tokens: None,
                    max_completion_tokens: None,
                },
                OpenAIModel {
                    id: "deepseek-expert".to_string(),
                    object: "model",
                    created: 1_090_108_800,
                    owned_by: "x",
                    max_input_tokens: None,
                    max_output_tokens: None,
                    context_length: None,
                    context_window: None,
                    max_context_length: None,
                    max_tokens: None,
                    max_completion_tokens: None,
                },
            ],
        }
    }

    #[test]
    fn list_maps_openai_models() {
        let resp = list(&sample_list());
        assert_eq!(resp.data.len(), 2);
        assert_eq!(resp.data[0].id, "deepseek-default");
        assert_eq!(resp.data[1].id, "deepseek-expert");
        assert_eq!(resp.data[0].ty, "model");
        assert!(resp.data[0].capabilities.thinking.supported);
        assert!(resp.data[0].capabilities.image_input.supported);
        assert!(resp.data[0].capabilities.pdf_input.supported);
        assert_eq!(resp.data[0].created_at, "2004-07-18T00:00:00Z");
        assert!(resp.data[0].max_input_tokens.is_none());
    }

    #[test]
    fn get_finds_existing_model() {
        let info = get(&sample_list(), "deepseek-default").unwrap();
        assert_eq!(info.id, "deepseek-default");
        assert_eq!(info.display_name, "Deepseek Default");
    }

    #[test]
    fn get_returns_none_for_missing_model() {
        let model_list = OpenAIModelList {
            object: "list",
            data: vec![],
        };
        assert!(get(&model_list, "deepseek-default").is_none());
    }

    #[test]
    fn list_handles_empty_data() {
        let resp = list(&OpenAIModelList {
            object: "list",
            data: vec![],
        });
        assert!(resp.data.is_empty());
        assert!(!resp.has_more);
    }
}
