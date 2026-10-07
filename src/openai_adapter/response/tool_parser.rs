//! 工具调用解析 —— 滑动窗口检测 `<tool_calls>...</tool_calls>`，转换为结构化 tool_calls
//!
//! 算法核心：
//! - Detecting 状态：维护固定宽度 W 的扫描缓冲区，新 chunk 到来时
//!   先追加到缓冲区，扫描 `<tool_calls>`（或回退 `<tool_call>`），未找到则释放超出 W 的安全部分
//! - CollectingXml 状态：检测到标记后收集内容直到 `</tool_calls>`
//! - Done 状态：工具调用已发出，截断后续内容（防幻觉）

use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use futures::Stream;
use pin_project_lite::pin_project;

use log::{debug, trace, warn};

use crate::openai_adapter::OpenAIAdapterError;
use crate::openai_adapter::types::{
    ChatCompletionsResponseChunk, ChunkChoice, Delta, FunctionCall, ToolCall,
};

static CALL_ID_COUNTER: AtomicU64 = AtomicU64::new(1);
pub(crate) const MAX_XML_BUF_LEN: usize = 64 * 1024;

pub(crate) const TOOL_CALL_START: &str = "<|tool▁calls▁begin|>";
pub(crate) const TOOL_CALL_END: &str = "<|tool▁calls▁end|>";
const W: usize = 71;

const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug, Clone)]
pub struct TagConfig {
    pub starts: Vec<String>,
    pub ends: Vec<String>,
}

impl TagConfig {
    pub fn from_config(cfg: &crate::config::ToolCallTagConfig) -> Self {
        Self {
            starts: cfg.extra_starts.clone(),
            ends: cfg.extra_ends.clone(),
        }
    }
}

/// 标签字符归一化：`｜`(U+FF5C) → `|`，`▁`(U+2581) → `_`
const fn norm_tag_char(c: char) -> char {
    match c {
        '\u{FF5C}' => '|',
        '\u{2581}' => '_',
        _ => c,
    }
}

/// 标签字符等价判断
const fn eq_tag_char(a: char, b: char) -> bool {
    a == b || norm_tag_char(a) == norm_tag_char(b)
}

/// 模糊匹配标签：在 `haystack` 中查找 `partial`，支持 `｜`↔`|`、`▁`↔`_` 等价
fn fuzzy_match_tag<'a>(haystack: &'a str, partial: &str) -> Option<(usize, &'a str)> {
    let n_chars: Vec<char> = partial.chars().collect();
    let h_chars: Vec<char> = haystack.chars().collect();

    if n_chars.is_empty() || h_chars.len() < n_chars.len() {
        return None;
    }

    for start in 0..=h_chars.len() - n_chars.len() {
        let mut matched = true;
        for j in 0..n_chars.len() {
            if !eq_tag_char(n_chars[j], h_chars[start + j]) {
                matched = false;
                break;
            }
        }
        if matched {
            let byte_pos: usize = h_chars[..start].iter().map(|c| c.len_utf8()).sum();
            let tag_len: usize = h_chars[start..start + n_chars.len()]
                .iter()
                .map(|c| c.len_utf8())
                .sum();
            return Some((byte_pos, &haystack[byte_pos..byte_pos + tag_len]));
        }
    }
    None
}

fn match_start_tag<'a>(s: &'a str, tag: &str) -> Option<(usize, &'a str)> {
    let partial = tag.trim_end_matches('>');
    s.find(partial)
        .map(|pos| (pos, &s[pos..pos + partial.len()]))
        .or_else(|| fuzzy_match_tag(s, partial))
}

pub(crate) fn contains_start_tag_with(s: &str, cfg: &TagConfig) -> bool {
    if match_start_tag(s, TOOL_CALL_START).is_some() {
        return true;
    }
    for start in &cfg.starts {
        if match_start_tag(s, start).is_some() {
            return true;
        }
    }
    false
}

pub(crate) fn find_start_tag_with<'a>(s: &'a str, cfg: &TagConfig) -> Option<(usize, &'a str)> {
    if let Some(m) = match_start_tag(s, TOOL_CALL_START) {
        return Some(m);
    }
    for start in &cfg.starts {
        if let Some(m) = match_start_tag(s, start) {
            return Some(m);
        }
    }
    None
}

pub(crate) fn find_end_tag_with<'a>(
    s: &'a str,
    from: usize,
    cfg: &TagConfig,
    start_tag: Option<&str>,
) -> Option<(usize, &'a str)> {
    let search = &s[from..];
    if let Some(st) = start_tag {
        let open_tag = st.trim_end_matches('>');
        let close_tag = format!("</{}>", &open_tag[1..]);
        if let Some(pos) = search.find(&close_tag) {
            let abs = from + pos;
            return Some((abs, &s[abs..abs + close_tag.len()]));
        }
        // 模糊回退：close_tag 中可能含 ｜/▁ 变体
        let close_partial = close_tag.trim_end_matches('>');
        if let Some((pos, matched)) = fuzzy_match_tag(search, close_partial) {
            let abs = from + pos;
            return Some((abs, &s[abs..abs + matched.len()]));
        }
    }

    // 无论 start_tag 是否提供，都尝试已知结束标签
    for end in std::iter::once(TOOL_CALL_END).chain(cfg.ends.iter().map(|s| s.as_str())) {
        if let Some(pos) = search.find(end) {
            let abs = from + pos;
            return Some((abs, &s[abs..abs + end.len()]));
        }
        // 模糊回退
        let end_partial = end.trim_end_matches('>');
        if let Some((pos, matched)) = fuzzy_match_tag(search, end_partial) {
            let abs = from + pos;
            return Some((abs, &s[abs..abs + matched.len()]));
        }
    }
    if let Some(st) = start_tag
        && let Some((pos, tag)) = match_start_tag(search, st)
    {
        return Some((from + pos, &s[from + pos..from + pos + tag.len()]));
    }
    if let Some((pos, tag)) = match_start_tag(search, TOOL_CALL_START) {
        return Some((from + pos, &s[from + pos..from + pos + tag.len()]));
    }
    for start in &cfg.starts {
        if let Some((pos, tag)) = match_start_tag(search, start) {
            return Some((from + pos, &s[from + pos..from + pos + tag.len()]));
        }
    }
    None
}

fn is_start_tag(tag: &str, cfg: &TagConfig) -> bool {
    if !tag.starts_with('<') {
        return false;
    }
    let partial = TOOL_CALL_START.trim_end_matches('>');
    let tag_norm: String = tag.chars().map(norm_tag_char).collect();
    let partial_norm: String = partial.chars().map(norm_tag_char).collect();
    if partial_norm.starts_with(&tag_norm) || tag_norm.starts_with(&partial_norm) {
        return true;
    }
    for start in &cfg.starts {
        let p: String = start
            .trim_end_matches('>')
            .chars()
            .map(norm_tag_char)
            .collect();
        if p.starts_with(&tag_norm) || tag_norm.starts_with(&p) {
            return true;
        }
    }
    false
}

fn next_call_id() -> String {
    let n = CALL_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("call_{n:016x}")
}

const fn floor_char_boundary(s: &str, max: usize) -> usize {
    if max >= s.len() {
        return s.len();
    }
    let mut i = max;
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn is_inside_code_fence(xml: &str, tag_pos: usize) -> bool {
    xml[..tag_pos].matches("```").count() % 2 == 1
}

fn repair_invalid_backslashes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.peek() {
                Some(&next)
                    if matches!(next, '"' | '\\' | '/' | 'b' | 'f' | 'n' | 'r' | 't' | 'u') =>
                {
                    out.push('\\');
                    out.push(next);
                    chars.next();
                }
                Some(&next) => {
                    out.push('\\');
                    out.push('\\');
                    out.push(next);
                    chars.next();
                }
                None => {
                    out.push('\\');
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn repair_unquoted_keys(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 32);
    let chars: Vec<char> = s.chars().collect();
    let len = chars.len();
    let mut i = 0;
    while i < len {
        // 两条分支都要原样输出当前字符，先统一输出再判断是否是键起始位置
        out.push(chars[i]);
        if (chars[i] == '{' || chars[i] == ',') && i + 1 < len {
            i += 1;
            while i < len && chars[i].is_whitespace() {
                out.push(chars[i]);
                i += 1;
            }
            if i < len && (chars[i].is_alphabetic() || chars[i] == '_') {
                let key_start = i;
                while i < len && (chars[i].is_alphanumeric() || chars[i] == '_') {
                    i += 1;
                }
                if i < len && chars[i] == ':' {
                    out.push('"');
                    out.extend(&chars[key_start..i]);
                    out.push('"');
                } else {
                    out.extend(&chars[key_start..i]);
                    continue;
                }
            }
        } else {
            i += 1;
        }
    }
    out
}

fn repair_json(s: &str) -> Option<String> {
    let step1 = repair_invalid_backslashes(s);
    if serde_json::from_str::<serde_json::Value>(&step1).is_ok() {
        return Some(step1);
    }
    let step2 = repair_unquoted_keys(&step1);
    if serde_json::from_str::<serde_json::Value>(&step2).is_ok() {
        return Some(step2);
    }
    None
}

pub fn parse_tool_calls(xml: &str) -> Option<(Vec<ToolCall>, String)> {
    parse_tool_calls_with(
        xml,
        &TagConfig::from_config(&crate::config::ToolCallTagConfig::default()),
    )
}

pub fn parse_tool_calls_with(xml: &str, cfg: &TagConfig) -> Option<(Vec<ToolCall>, String)> {
    let (start, start_tag) = find_start_tag_with(xml, cfg)?;
    let after_start = start + start_tag.len();
    if is_inside_code_fence(xml, start) {
        return None;
    }

    let (end, inner_end) = match find_end_tag_with(xml, after_start, cfg, Some(start_tag)) {
        Some((pos, matched_end)) => (pos + matched_end.len(), pos),
        None => (xml.len(), xml.len()),
    };
    let inner = &xml[after_start..inner_end];

    let arr = match inner.find('[') {
        Some(arr_start) => {
            let arr_end = inner.rfind(']').map_or(inner.len(), |p| p + 1);
            let json_str = &inner[arr_start..arr_end];
            if json_str.trim() == "[]" {
                return None;
            }
            if let Ok(a) = serde_json::from_str::<Vec<serde_json::Value>>(json_str) {
                a
            } else {
                let repaired = repair_json(json_str).unwrap_or_default();
                let obj_str = repaired.trim_start_matches('[');
                let obj_start = obj_str.find('{')?;
                let obj_end = obj_str.rfind('}').map_or(obj_str.len(), |p| p + 1);
                serde_json::from_str(&obj_str[obj_start..obj_end])
                    .ok()
                    .filter(|v: &serde_json::Value| v.is_object())
                    .map(|v| vec![v])?
            }
        }
        None => {
            if let Some(obj_start) = inner.find('{') {
                let obj_end = inner.rfind('}').map_or(inner.len(), |p| p + 1);
                let json_str = &inner[obj_start..obj_end];
                let obj = serde_json::from_str(json_str)
                    .ok()
                    .filter(|v: &serde_json::Value| v.is_object())
                    .or_else(|| {
                        let repaired = repair_json(json_str)?;
                        serde_json::from_str(&repaired)
                            .ok()
                            .filter(|v: &serde_json::Value| v.is_object())
                    })?;
                vec![obj]
            } else {
                return parse_invoke_calls(inner, &xml[..start], &xml[end..]);
            }
        }
    };

    let mut calls = Vec::new();
    for item in arr {
        let name = item.get("name")?.as_str()?.to_string();
        let arguments = item.get("arguments").map_or_else(
            || "{}".into(),
            |v| {
                v.as_str().map_or_else(
                    || serde_json::to_string(v).unwrap_or_else(|_| "{}".into()),
                    |s| {
                        serde_json::from_str::<serde_json::Value>(s)
                            .ok()
                            .and_then(|obj| serde_json::to_string(&obj).ok())
                            .unwrap_or_else(|| s.to_string())
                    },
                )
            },
        );
        calls.push(ToolCall {
            id: next_call_id(),
            ty: "function".to_string(),
            function: Some(FunctionCall { name, arguments }),
            custom: None,
            index: u32::try_from(calls.len()).unwrap_or(u32::MAX),
        });
    }
    if calls.is_empty() {
        return None;
    }
    let remaining = format!("{}{}", &xml[..start], &xml[end..]);
    Some((calls, remaining))
}

fn parse_invoke_calls(inner: &str, prefix: &str, suffix: &str) -> Option<(Vec<ToolCall>, String)> {
    use std::collections::BTreeMap;
    let mut calls = Vec::new();
    let mut pos = 0;
    let lower = inner.to_lowercase();
    while let Some(invoke_start) = lower[pos..].find("<invoke ") {
        let abs_start = pos + invoke_start;
        let name_attr = &inner[abs_start..];
        let name_start = name_attr.find("name=\"")? + 6;
        let name_end = name_attr[name_start..].find('"')?;
        let name = &name_attr[name_start..name_start + name_end];
        let close_tag = "</invoke>";
        let rest = &lower[abs_start..];
        let close_pos = rest.find(close_tag)?;
        let invoke_body = &inner[abs_start..abs_start + close_pos + close_tag.len()];
        let mut params: BTreeMap<String, serde_json::Value> = BTreeMap::new();
        let mut ppos = 0;
        let body_lower = invoke_body.to_lowercase();
        while let Some(p_start) = body_lower[ppos..].find("<parameter ") {
            let p_abs = ppos + p_start;
            let p_attr = &invoke_body[p_abs..];
            let p_name_start = p_attr.find("name=\"")? + 6;
            let p_name_end = p_attr[p_name_start..].find('"')?;
            let p_name = &p_attr[p_name_start..p_name_start + p_name_end];
            let p_body_start = p_attr.find('>')? + 1;
            let p_close = String::from("</parameter>");
            let p_close_pos = p_attr[p_body_start..].find(&p_close)?;
            let p_value = &p_attr[p_body_start..p_body_start + p_close_pos];
            let val: serde_json::Value = serde_json::from_str(p_value.trim())
                .unwrap_or_else(|_| serde_json::Value::String(p_value.to_string()));
            params.insert(p_name.to_string(), val);
            let p_end = p_body_start + p_close_pos + p_close.len();
            ppos += p_start + p_end;
        }
        let arguments = serde_json::to_string(&params).unwrap_or_else(|_| "{}".into());
        calls.push(ToolCall {
            id: next_call_id(),
            ty: "function".to_string(),
            function: Some(FunctionCall {
                name: name.to_string(),
                arguments,
            }),
            custom: None,
            index: u32::try_from(calls.len()).unwrap_or(u32::MAX),
        });
        pos = abs_start + close_pos + close_tag.len();
    }
    if calls.is_empty() {
        return None;
    }
    Some((calls, format!("{prefix}{suffix}")))
}

fn make_end_chunk(
    model: &str,
    delta: Delta,
    finish_reason: &'static str,
) -> ChatCompletionsResponseChunk {
    ChatCompletionsResponseChunk {
        id: "chatcmpl-end".to_string(),
        object: "chat.completion.chunk",
        created: 0,
        model: model.to_string(),
        choices: vec![ChunkChoice {
            index: 0,
            delta,
            finish_reason: Some(finish_reason),
            logprobs: None,
        }],
        usage: None,
        service_tier: None,
        system_fingerprint: None,
        obfuscation: None,
    }
}

#[derive(Debug)]
enum ToolParseState {
    Detecting { buffer: String },
    CollectingXml { buf: String, start_tag: String },
    Done,
}

pin_project! {
    pub struct ToolCallStream<S> {
        #[pin]
        inner: S,
        state: ToolParseState,
        model: String,
        finish_emitted: bool,
        repair_pending: Option<String>,
        tag_config: Arc<TagConfig>,
        last_keepalive: tokio::time::Instant,
    }
}

impl<S> ToolCallStream<S> {
    pub fn new(inner: S, model: String, tag_config: Arc<TagConfig>) -> Self {
        Self {
            inner,
            state: ToolParseState::Detecting {
                buffer: String::new(),
            },
            model,
            finish_emitted: false,
            repair_pending: None,
            tag_config,
            last_keepalive: tokio::time::Instant::now(),
        }
    }
}

impl<S> Stream for ToolCallStream<S>
where
    S: Stream<Item = Result<ChatCompletionsResponseChunk, OpenAIAdapterError>>,
{
    type Item = Result<ChatCompletionsResponseChunk, OpenAIAdapterError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut this = self.project();

        if let Some(tool_text) = this.repair_pending.take() {
            debug!(target: "adapter", "tool_parser requesting repair");
            return Poll::Ready(Some(Err(OpenAIAdapterError::ToolCallRepairNeeded(
                tool_text,
            ))));
        }

        loop {
            if matches!(&this.state, ToolParseState::CollectingXml { .. })
                && this.last_keepalive.elapsed() >= KEEPALIVE_INTERVAL
            {
                // 保活：XML 收集期间上游可能长时间无输出，这里发一个**空 delta** 心跳，
                // 只为让连接不至于空闲超时。
                //
                // 早期实现发的是 `tool_calls: [{id:"", name:"", arguments:""}]`，
                // 客户端按 index/id 累积工具调用时会把它当成一个个新的空工具调用，
                // 表现为「反复工具调用」（issue #87）。空 delta 不携带任何
                // tool_calls/content，客户端会自然忽略。
                trace!(target: "adapter", ">>> keepalive: sending empty delta");
                *this.last_keepalive = tokio::time::Instant::now();
                return Poll::Ready(Some(Ok(ChatCompletionsResponseChunk {
                    id: "chatcmpl-keepalive".into(),
                    object: "chat.completion.chunk",
                    created: 0,
                    model: this.model.clone(),
                    choices: vec![ChunkChoice {
                        index: 0,
                        delta: Delta::default(),
                        finish_reason: None,
                        logprobs: None,
                    }],
                    usage: None,
                    service_tier: None,
                    system_fingerprint: None,
                    obfuscation: None,
                })));
            }

            match this.inner.as_mut().poll_next(cx) {
                Poll::Ready(Some(Ok(mut chunk))) => {
                    let Some(choice) = chunk.choices.first_mut() else {
                        return Poll::Ready(Some(Ok(chunk)));
                    };

                    if let Some(content) = choice.delta.content.take() {
                        if content.is_empty() {
                            choice.delta.content = Some(content);
                            return Poll::Ready(Some(Ok(chunk)));
                        }

                        match &mut this.state {
                            ToolParseState::Detecting { buffer } => {
                                buffer.push_str(&content);

                                let maybe_tag = find_start_tag_with(buffer, this.tag_config)
                                    .map(|(pos, tag)| (pos, tag.to_string()));
                                if let Some((pos, start_tag)) = maybe_tag {
                                    trace!(target: "adapter", ">>> detected start_tag={}, buf_len={}", start_tag, buffer.len());
                                    let before = buffer[..pos].to_string();
                                    let rest = std::mem::take(buffer)[pos..].to_string();
                                    if let Some((end_pos, matched_end)) = find_end_tag_with(
                                        &rest,
                                        start_tag.len(),
                                        this.tag_config,
                                        Some(&start_tag),
                                    ) {
                                        let inner = &rest[start_tag.len()..end_pos];
                                        if is_start_tag(matched_end, this.tag_config)
                                            && inner.trim().is_empty()
                                        {
                                            if !before.is_empty() {
                                                choice.delta.content = Some(before);
                                            }
                                            *this.state = ToolParseState::CollectingXml {
                                                buf: rest,
                                                start_tag,
                                            };
                                            continue;
                                        }
                                        let end_abs = end_pos + matched_end.len();
                                        let collected = &rest[..end_abs];
                                        if let Some((calls, _)) = parse_tool_calls(collected) {
                                            debug!(target: "adapter", "tool_parser parsed {} tool call(s)", calls.len());
                                            choice.delta.content = if before.is_empty() {
                                                None
                                            } else {
                                                Some(before)
                                            };
                                            choice.delta.tool_calls = Some(calls);
                                            if choice.finish_reason == Some("stop") {
                                                choice.finish_reason = Some("tool_calls");
                                            }
                                            *this.state = ToolParseState::Done;
                                        } else {
                                            trace!(target: "adapter", "tool_parser parse failed, collected=\n{}", &collected[..floor_char_boundary(collected, 500)]);
                                            warn!(target: "adapter", "tool_parser parse failed -> requesting repair");
                                            let collected = collected.to_string();
                                            if before.is_empty() {
                                                return Poll::Ready(Some(Err(
                                                    OpenAIAdapterError::ToolCallRepairNeeded(
                                                        collected,
                                                    ),
                                                )));
                                            }
                                            choice.delta.content = Some(before);
                                            *this.repair_pending = Some(collected);
                                        }
                                        return Poll::Ready(Some(Ok(chunk)));
                                    }
                                    if before.is_empty() {
                                        *this.state = ToolParseState::CollectingXml {
                                            buf: rest,
                                            start_tag,
                                        };
                                        continue;
                                    }
                                    choice.delta.content = Some(before);
                                    *this.state = ToolParseState::CollectingXml {
                                        buf: rest,
                                        start_tag,
                                    };
                                    return Poll::Ready(Some(Ok(chunk)));
                                }
                                let safe =
                                    floor_char_boundary(buffer, buffer.len().saturating_sub(W));
                                if safe > 0 {
                                    choice.delta.content = Some(buffer[..safe].to_string());
                                    buffer.drain(..safe);
                                    return Poll::Ready(Some(Ok(chunk)));
                                }
                                continue;
                            }

                            ToolParseState::CollectingXml { buf, start_tag } => {
                                buf.push_str(&content);
                                if buf.len() > MAX_XML_BUF_LEN {
                                    debug!(target: "adapter", "tool_parser buffer overflow, falling back to plain text");
                                    let flushed = std::mem::take(buf);
                                    *this.state = ToolParseState::Detecting {
                                        buffer: String::new(),
                                    };
                                    choice.delta.content = Some(flushed);
                                    return Poll::Ready(Some(Ok(chunk)));
                                }
                                let start_end = buf.find('>').map_or(0, |p| p + 1);
                                if let Some((end_pos, en_tag)) = find_end_tag_with(
                                    buf,
                                    start_end,
                                    this.tag_config,
                                    Some(start_tag),
                                ) {
                                    let inner = &buf[start_end..end_pos];
                                    if is_start_tag(en_tag, this.tag_config)
                                        && inner.trim().is_empty()
                                    {
                                        continue;
                                    }
                                    let end_abs = end_pos + en_tag.len();
                                    let collected = buf[..end_abs].to_string();
                                    let _tail = buf.split_off(end_abs);
                                    if let Some((calls, _)) = parse_tool_calls(&collected) {
                                        debug!(target: "adapter", "tool_parser parsed {} tool call(s)", calls.len());
                                        choice.delta.content = None;
                                        choice.delta.tool_calls = Some(calls);
                                        if choice.finish_reason == Some("stop") {
                                            choice.finish_reason = Some("tool_calls");
                                        }
                                        *this.state = ToolParseState::Done;
                                    } else {
                                        trace!(target: "adapter", "tool_parser parse failed (stream end), collected=\n{}", &collected[..floor_char_boundary(&collected, 500)]);
                                        warn!(target: "adapter", "tool_parser parse failed -> requesting repair");
                                        return Poll::Ready(Some(Err(
                                            OpenAIAdapterError::ToolCallRepairNeeded(collected),
                                        )));
                                    }
                                    return Poll::Ready(Some(Ok(chunk)));
                                }
                                continue;
                            }

                            ToolParseState::Done => {
                                // 工具调用已发出，后续内容一律丢弃（防幻觉）。
                                //
                                // 注意：这里不能提前发出结束 chunk。上游在工具调用
                                // 之后还会送出一个带 finish_reason 与 usage 的收尾
                                // chunk，若在此处就结束流，usage 会被丢掉
                                // （表现为 tool_calls 场景 completion_tokens 恒为 0）。
                                // 交给下面带 finish_reason 的分支或流结束分支补发。
                                continue;
                            }
                        }
                    }
                    match &mut this.state {
                        ToolParseState::Detecting { buffer } => {
                            if choice.finish_reason.is_some() {
                                if !buffer.is_empty() {
                                    choice.delta.content = Some(std::mem::take(buffer));
                                }
                                return Poll::Ready(Some(Ok(chunk)));
                            }
                            return Poll::Ready(Some(Ok(chunk)));
                        }
                        ToolParseState::CollectingXml { buf, start_tag: _ } => {
                            if choice.finish_reason.is_some() {
                                let flushed = std::mem::take(buf);
                                if let Some((calls, _)) = parse_tool_calls(&flushed) {
                                    debug!(target: "adapter", "tool_parser parsed {} tool call(s) at stream end", calls.len());
                                    choice.delta.tool_calls = Some(calls);
                                    if choice.finish_reason == Some("stop") {
                                        choice.finish_reason = Some("tool_calls");
                                    }
                                } else {
                                    warn!(target: "adapter", "tool_parser finish -> requesting repair");
                                    *this.state = ToolParseState::Done;
                                    return Poll::Ready(Some(Err(
                                        OpenAIAdapterError::ToolCallRepairNeeded(flushed),
                                    )));
                                }
                                *this.state = ToolParseState::Done;
                                return Poll::Ready(Some(Ok(chunk)));
                            }
                            return Poll::Ready(Some(Ok(chunk)));
                        }
                        ToolParseState::Done => {
                            if !*this.finish_emitted {
                                *this.finish_emitted = true;
                                let mut end =
                                    make_end_chunk(this.model, Delta::default(), "tool_calls");
                                if let Some(ref u) = chunk.usage {
                                    end.usage = Some(u.clone());
                                }
                                return Poll::Ready(Some(Ok(end)));
                            }
                            return Poll::Ready(None);
                        }
                    }
                }
                Poll::Ready(Some(Err(e))) => return Poll::Ready(Some(Err(e))),
                Poll::Ready(None) => match std::mem::replace(this.state, ToolParseState::Done) {
                    ToolParseState::Detecting { buffer } => {
                        if !buffer.is_empty() {
                            let chunk = make_end_chunk(
                                this.model,
                                Delta {
                                    content: Some(buffer),
                                    ..Default::default()
                                },
                                "stop",
                            );
                            return Poll::Ready(Some(Ok(chunk)));
                        }
                        return Poll::Ready(None);
                    }
                    ToolParseState::CollectingXml { buf, start_tag: _ } => {
                        if let Some((calls, _)) = parse_tool_calls(&buf) {
                            debug!(target: "adapter", "tool_parser parsed {} tool call(s) at stream end", calls.len());
                            let chunk = make_end_chunk(
                                this.model,
                                Delta {
                                    tool_calls: Some(calls),
                                    ..Default::default()
                                },
                                "tool_calls",
                            );
                            return Poll::Ready(Some(Ok(chunk)));
                        }
                        warn!(target: "adapter", "tool_parser stream end -> requesting repair");
                        return Poll::Ready(Some(Err(OpenAIAdapterError::ToolCallRepairNeeded(
                            buf,
                        ))));
                    }
                    ToolParseState::Done => {
                        // 上游未发送带 finish_reason 的收尾 chunk 就断流：
                        // 补发一个结束 chunk，保证下游一定能看到 tool_calls 终止信号。
                        if !*this.finish_emitted {
                            *this.finish_emitted = true;
                            return Poll::Ready(Some(Ok(make_end_chunk(
                                this.model,
                                Delta::default(),
                                "tool_calls",
                            ))));
                        }
                        return Poll::Ready(None);
                    }
                },
                Poll::Pending => break,
            }
        }
        Poll::Pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(content: &str) -> String {
        format!("{TOOL_CALL_START}{content}{TOOL_CALL_END}")
    }
    fn tool_ts(content: &str, suffix: &str) -> String {
        format!("{TOOL_CALL_START}{content}{TOOL_CALL_END}{suffix}")
    }

    /// 回归：trace 日志预览用 floor_char_boundary 截断，不得切开多字节字符
    #[test]
    fn floor_char_boundary_never_splits_multibyte() {
        let s = "中".repeat(300); // 900 字节，索引 500 落在字符内部
        let idx = floor_char_boundary(&s, 500);
        assert!(s.is_char_boundary(idx));
        assert!(idx <= 500);
        let _ = &s[..idx];
    }

    #[test]
    fn parse_json_tool_calls() {
        let xml = tool(r#"[{"name": "get_weather", "arguments": {"city": "北京"}}]"#);
        let (calls, remaining) = parse_tool_calls(&xml).unwrap();
        assert!(remaining.is_empty());
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.as_ref().unwrap().name, "get_weather");
        assert_eq!(
            calls[0].function.as_ref().unwrap().arguments,
            r#"{"city":"北京"}"#
        );
    }

    #[test]
    fn parse_json_with_surrounding_text() {
        let xml = format!(
            "{TOOL_CALL_START}\n\t以下是工具调用：\n\t[{{\"name\": \"f\", \"arguments\": {{}}}}]\n\t{TOOL_CALL_END}"
        );
        let (calls, _) = parse_tool_calls(&xml).unwrap();
        assert_eq!(calls.len(), 1);
    }

    #[test]
    fn parse_json_multiple_tools() {
        let xml = tool(
            r#"[{"name": "get_weather", "arguments": {}}, {"name": "get_time", "arguments": {"tz": "bj"}}]"#,
        );
        let (calls, remaining) = parse_tool_calls(&xml).unwrap();
        assert!(remaining.is_empty());
        assert_eq!(calls.len(), 2);
    }

    #[test]
    fn parse_json_with_trailing_text() {
        let xml = tool_ts(
            r#"[{"name": "get_weather", "arguments": {}}]"#,
            " trailing text",
        );
        let (calls, remaining) = parse_tool_calls(&xml).unwrap();
        assert_eq!(remaining, " trailing text");
        assert_eq!(calls.len(), 1);
    }

    #[test]
    fn repair_backslashes_passes_valid_escapes() {
        assert_eq!(repair_invalid_backslashes(r"hello\nworld"), r"hello\nworld");
    }
    #[test]
    fn repair_backslashes_fixes_invalid_escapes() {
        assert_eq!(repair_invalid_backslashes(r"C:\Users\name").len(), 14);
    }
    #[test]
    fn repair_backslashes_keeps_valid_n() {
        assert_eq!(repair_invalid_backslashes(r"line1\nline2"), r"line1\nline2");
    }
    #[test]
    fn repair_unquoted_keys_basic() {
        assert_eq!(
            repair_unquoted_keys(r#"{name: "get_weather"}"#),
            r#"{"name": "get_weather"}"#
        );
    }
    #[test]
    fn repair_unquoted_keys_array() {
        assert_eq!(
            repair_unquoted_keys(r#"[{name: "f", arguments: {}}]"#),
            r#"[{"name": "f", "arguments": {}}]"#
        );
    }

    #[test]
    fn parse_tool_calls_with_unquoted_keys() {
        let xml = tool(r#"[{name: "get_weather", arguments: {city: "北京"}}]"#);
        let (calls, _) = parse_tool_calls(&xml).unwrap();
        assert_eq!(calls.len(), 1);
    }

    #[test]
    fn parse_tool_calls_with_invalid_backslashes() {
        let xml = tool(r#"[{"name": "read_file", "arguments": {"path": "C:\Users\name"}}]"#);
        let (calls, _) = parse_tool_calls(&xml).unwrap();
        assert_eq!(calls.len(), 1);
    }

    #[test]
    fn parse_tool_calls_with_both_repairs() {
        let xml = tool(r#"[{name: "read_file", arguments: {path: "C:\file"}}]"#);
        let (calls, _) = parse_tool_calls(&xml).unwrap();
        assert_eq!(calls.len(), 1);
    }

    #[test]
    fn parse_tool_calls_inside_code_fence_skipped() {
        let xml = format!(
            "示例：\n```json\n{TOOL_CALL_START}[{{\"name\": \"get_weather\", \"arguments\": {{}}}}]{TOOL_CALL_END}\n```"
        );
        assert!(parse_tool_calls(&xml).is_none());
    }

    #[test]
    fn parse_tool_calls_not_inside_code_fence() {
        assert!(parse_tool_calls(&tool(r#"[{"name": "get_weather", "arguments": {}}]"#)).is_some());
    }

    #[test]
    fn parse_tool_calls_tool_call_inside_value_not_skipped() {
        let xml = tool(
            r#"[{"name": "format_code", "arguments": {"code": "```rust\nfn main() {}\n```"}}]"#,
        );
        let (calls, _) = parse_tool_calls(&xml).unwrap();
        assert_eq!(calls.len(), 1);
    }

    #[test]
    fn code_fence_detection() {
        assert!(!is_inside_code_fence("普通文本", 0));
    }

    #[test]
    fn parse_tool_calls_single_object() {
        let xml = tool(r#"{"name": "get_weather", "arguments": {"city": "北京"}}"#);
        let (calls, _) = parse_tool_calls(&xml).unwrap();
        assert_eq!(calls.len(), 1);
    }

    #[test]
    fn parse_tool_calls_single_object_with_newlines() {
        let xml = format!(
            "{TOOL_CALL_START}\n{{\"name\": \"Bash\", \"arguments\": {{\"command\": \"ls\"}}}}\n{TOOL_CALL_END}"
        );
        let (calls, _) = parse_tool_calls(&xml).unwrap();
        assert_eq!(calls.len(), 1);
    }

    #[test]
    fn parse_tool_calls_single_object_with_surrounding_text() {
        let xml = format!(
            "{TOOL_CALL_START}以下是工具调用：{{\"name\": \"f\", \"arguments\": {{}}}}{TOOL_CALL_END}"
        );
        let (_calls, remaining) = parse_tool_calls(&xml).unwrap();
        assert_eq!(remaining, "");
    }

    #[test]
    fn parse_tool_calls_single_object_unquoted_keys() {
        let xml = tool(r#"{name: "get_weather", arguments: {city: "北京"}}"#);
        let (calls, _) = parse_tool_calls(&xml).unwrap();
        assert_eq!(calls.len(), 1);
    }

    #[test]
    fn parse_tool_calls_single_object_and_repair_backslashes() {
        let xml = tool(r#"{"name": "read_file", "arguments": {"path": "C:\Users\name"}}"#);
        let (calls, _) = parse_tool_calls(&xml).unwrap();
        assert_eq!(calls.len(), 1);
    }

    #[test]
    fn fuzzy_match_hallucinated_marker() {
        // <|tool▁calls▁begin|> 正常标签，但结束标签用 <|tool_calls▁end｜>
        // （ASCII _ + ▁ + 全角 ｜），验证模糊匹配能识别
        let xml = format!(
            r#"{TOOL_CALL_START}[{{"name": "get_weather", "arguments": {{"city": "北京"}}}}]<|tool_calls▁end｜>"#
        );
        let (calls, _) = parse_tool_calls(&xml).unwrap();
        assert_eq!(calls.len(), 1);
    }

    /// 回归（issue #87「反复工具调用」）：保活心跳必须是空 delta。
    ///
    /// 早期实现每秒发一个 `tool_calls: [{id:"", name:"", arguments:""}]`，
    /// 客户端按 index/id 累积工具调用时会得到一串空工具调用，
    /// 表现为反复调用工具。
    #[tokio::test]
    async fn keepalive_emits_empty_delta() {
        use futures::StreamExt;

        // 构造一个永不产出数据的内部流，且让状态停在 CollectingXml，
        // 这样 poll_next 会走到保活分支。
        let inner: Pin<
            Box<dyn Stream<Item = Result<ChatCompletionsResponseChunk, OpenAIAdapterError>> + Send>,
        > = Box::pin(futures::stream::pending());

        let mut parser = ToolCallStream::new(
            inner,
            "deepseek-default".into(),
            std::sync::Arc::new(TagConfig::from_config(
                &crate::config::ToolCallTagConfig::default(),
            )),
        );
        // 手动把状态推进到 CollectingXml
        parser.state = ToolParseState::CollectingXml {
            buf: String::new(),
            start_tag: TOOL_CALL_START.to_string(),
        };
        // 让上一次心跳时间早于阈值
        parser.last_keepalive = tokio::time::Instant::now() - KEEPALIVE_INTERVAL * 2;

        let chunk = parser.next().await.expect("应产生保活 chunk").unwrap();
        let delta = &chunk.choices[0].delta;
        assert!(
            delta.tool_calls.is_none(),
            "保活心跳不得携带 tool_calls（会诱发 issue #87 的反复工具调用）: {:?}",
            delta.tool_calls
        );
        assert!(delta.content.is_none(), "保活心跳不得携带 content");
    }
}
