//! OpenAI 模型列表响应生成
//!
//! 基于 DeepSeek `model_types` + `model_aliases` 静态生成 OpenAI `/models` 响应。
//!
//! # 单一事实来源
//!
//! `list()` 与 `get()` 必须对外暴露**完全一致**的模型 ID 集合，否则会出现
//! 「列表里有、按 ID 查询却拿不到」或「查询返回的 id 与列表里的拼写不同」。
//!
//! 早期实现把「生成 ID」的逻辑在两处各写一遍，已经产生过真实缺陷：
//! 别名查询返回的是**小写化后的** ID（`make_model(&target, ..)`），
//! 而列表返回的是别名的**原始大小写**，同一个模型因此有两个 ID。
//!
//! 现在由一个函数 [`model_ids`] 统一生成有序 ID 列表，`list()` 与 `get()` 共用它。

use crate::openai_adapter::types::{OpenAIModel, OpenAIModelList};

const MODEL_CREATED: u64 = 1_090_108_800;
const MODEL_OWNED_BY: &str = "deepseek-web (proxied by https://github.com/NIyueeE)";

/// 枚举全部可用的模型 ID（按展示顺序），值为 `model_types` 中的下标
///
/// 每个 `model_type` 会产生多个别名写法，便于客户端用不同约定引用同一模型：
/// - `deepseek-{ty}`（标准 ID）
/// - `{ty}`（裸 model_type 名；Claude Code / Codex 依赖此写法，见 issue #99）
/// - `model_aliases[i]`（用户自定义别名，可选）
///
/// `deepseek-{ty}` 与 `{ty}` 在忽略大小写后可能相同（例如 `ty = "DeepSeek-X"`），
/// 因此这里做去重，保证列表内 ID 唯一。
#[must_use]
pub fn model_ids(model_types: &[String], aliases: &[String]) -> Vec<(String, usize)> {
    let mut ids: Vec<(String, usize)> = Vec::with_capacity(model_types.len() * 2 + aliases.len());
    let mut seen: Vec<String> = Vec::with_capacity(ids.capacity());

    let push = |id: String, idx: usize, ids: &mut Vec<(String, usize)>, seen: &mut Vec<String>| {
        let key = id.to_lowercase();
        if !seen.contains(&key) {
            seen.push(key);
            ids.push((id, idx));
        }
    };

    // 1) 标准 ID：deepseek-<type>
    for (idx, ty) in model_types.iter().enumerate() {
        push(format!("deepseek-{ty}"), idx, &mut ids, &mut seen);
    }

    // 2) 用户别名（按 index 对齐 model_types）
    for (idx, alias) in aliases.iter().enumerate() {
        if model_types
            .get(idx)
            .is_some_and(|_| !alias.trim().is_empty())
        {
            push(alias.clone(), idx, &mut ids, &mut seen);
        }
    }

    // 3) 裸 model_type 名
    for (idx, ty) in model_types.iter().enumerate() {
        push(ty.clone(), idx, &mut ids, &mut seen);
    }

    ids
}

/// 根据 `model_types` + `aliases` 生成模型列表
pub fn list(
    model_types: &[String],
    max_input_tokens: &[u32],
    max_output_tokens: &[u32],
    aliases: &[String],
) -> OpenAIModelList {
    let data = model_ids(model_types, aliases)
        .into_iter()
        .map(|(id, idx)| {
            make_model(
                &id,
                max_input_tokens.get(idx).copied(),
                max_output_tokens.get(idx).copied(),
            )
        })
        .collect();

    OpenAIModelList {
        object: "list",
        data,
    }
}

/// 查询单个模型（ID 匹配忽略大小写，返回值使用列表中的规范拼写）
pub fn get(
    model_types: &[String],
    max_input_tokens: &[u32],
    max_output_tokens: &[u32],
    aliases: &[String],
    id: &str,
) -> Option<OpenAIModel> {
    let target = id.trim().to_lowercase();

    // 复用与 list() 完全相同的 ID 集合，杜绝两者漂移
    let (canonical, idx) = model_ids(model_types, aliases)
        .into_iter()
        .find(|(candidate, _)| candidate.to_lowercase() == target)?;

    Some(make_model(
        &canonical,
        max_input_tokens.get(idx).copied(),
        max_output_tokens.get(idx).copied(),
    ))
}

fn make_model(id: &str, input: Option<u32>, output: Option<u32>) -> OpenAIModel {
    OpenAIModel {
        id: id.to_string(),
        object: "model",
        created: MODEL_CREATED,
        owned_by: MODEL_OWNED_BY,
        max_input_tokens: input,
        max_output_tokens: output,
        context_length: input,
        context_window: input,
        max_context_length: input,
        max_tokens: output,
        max_completion_tokens: output,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn types() -> Vec<String> {
        vec!["default".to_string(), "expert".to_string()]
    }
    fn limits() -> (Vec<u32>, Vec<u32>) {
        (vec![100, 200], vec![10, 20])
    }

    #[test]
    fn list_includes_prefixed_and_bare_names() {
        let (mi, mo) = limits();
        let list = list(&types(), &mi, &mo, &[]);
        let ids: Vec<&str> = list.data.iter().map(|m| m.id.as_str()).collect();
        assert!(ids.contains(&"deepseek-default"));
        assert!(ids.contains(&"default"), "裸 model_type 名必须在列表中");
        assert!(ids.contains(&"deepseek-expert"));
        assert!(ids.contains(&"expert"));
    }

    #[test]
    fn get_accepts_prefixed_and_bare_names() {
        let (mi, mo) = limits();
        for id in ["deepseek-default", "DEFAULT", "default", "deepseek-expert"] {
            assert!(get(&types(), &mi, &mo, &[], id).is_some(), "{id} 应可查询");
        }
        assert!(get(&types(), &mi, &mo, &[], "nope").is_none());
    }

    #[test]
    fn get_accepts_aliases() {
        let (mi, mo) = limits();
        let aliases = vec!["gpt-4o".to_string()];
        let model = get(&types(), &mi, &mo, &aliases, "gpt-4o").expect("alias must resolve");
        assert_eq!(model.max_input_tokens, Some(100));
    }

    #[test]
    fn list_does_not_duplicate_alias_equal_to_type_name() {
        let (mi, mo) = limits();
        let aliases = vec!["default".to_string()];
        let list = list(&types(), &mi, &mo, &aliases);
        let count = list
            .data
            .iter()
            .filter(|m| m.id.eq_ignore_ascii_case("default"))
            .count();
        // "default"（别名或裸名）与 "deepseek-default" 只应各出现一次
        assert_eq!(count, 1, "别名与 model_type 同名时不应重复列出");
    }

    /// 回归：`get()` 返回的 ID 必须与 `list()` 中的拼写**逐字节相同**
    ///
    /// 旧实现对别名返回小写化后的 ID，导致同一模型在列表与查询结果里有两个 ID。
    #[test]
    fn get_returns_same_spelling_as_list_for_every_listed_id() {
        let (mi, mo) = limits();
        let aliases = vec!["MyModel".to_string(), String::new()];
        let listed = list(&types(), &mi, &mo, &aliases);

        for item in &listed.data {
            let fetched = get(&types(), &mi, &mo, &aliases, &item.id)
                .unwrap_or_else(|| panic!("列表中的 {} 必须可被 get 查询到", item.id));
            assert_eq!(
                fetched.id, item.id,
                "get() 返回的 id 必须与列表一致（大小写敏感）"
            );
            // 忽略大小写的查询也应返回**规范拼写**，而不是查询参数本身
            let lower = get(&types(), &mi, &mo, &aliases, &item.id.to_lowercase())
                .expect("小写查询也必须命中");
            assert_eq!(lower.id, item.id, "小写查询应返回规范拼写");
        }
    }

    #[test]
    fn mixed_case_alias_keeps_original_spelling() {
        let (mi, mo) = limits();
        let aliases = vec!["GPT-4o-Mini".to_string()];
        let fetched = get(&types(), &mi, &mo, &aliases, "gpt-4o-mini").unwrap();
        assert_eq!(
            fetched.id, "GPT-4o-Mini",
            "不应把小写查询参数当作规范 ID 返回"
        );
    }

    #[test]
    fn aliases_shorter_than_model_types_are_tolerated() {
        let (mi, mo) = limits();
        let aliases = vec!["only-first".to_string()];
        let list = list(&types(), &mi, &mo, &aliases);
        assert!(list.data.iter().any(|m| m.id == "only-first"));
        assert!(list.data.iter().any(|m| m.id == "deepseek-expert"));
    }

    #[test]
    fn blank_alias_is_ignored() {
        let aliases = vec!["  ".to_string(), String::new()];
        let ids: Vec<String> = model_ids(&types(), &aliases)
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert!(
            !ids.iter().any(|id| id.trim().is_empty()),
            "空白别名不得成为模型 ID：{ids:?}"
        );
    }

    #[test]
    fn no_duplicate_ids_when_type_already_prefixed() {
        // model_type 写成 "deepseek-x" 时，deepseek- 前缀会重复
        let types = vec!["deepseek-x".to_string()];
        let ids = model_ids(&types, &[]);
        let unique: std::collections::HashSet<String> =
            ids.iter().map(|(id, _)| id.to_lowercase()).collect();
        assert_eq!(ids.len(), unique.len(), "ID 列表必须无重复：{ids:?}");
    }

    #[test]
    fn canonical_spelling_survives_round_trip_through_list_and_get() {
        let (mi, mo) = limits();
        let aliases = vec!["Alias-Mixed-Case".to_string()];
        // list 中的每一项都能被 get 以同一拼写取回
        for item in list(&types(), &mi, &mo, &aliases).data {
            assert_eq!(
                get(&types(), &mi, &mo, &aliases, &item.id).unwrap().id,
                item.id
            );
        }
    }
}
