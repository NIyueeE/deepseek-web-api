//! 流式响应映射 —— 将 ChatCompletionsResponseChunk 流映射为 MessagesResponseChunk 流

use std::collections::VecDeque;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures::Stream;
use log::{debug, trace};
use pin_project_lite::pin_project;

use crate::anthropic_compat::AnthropicCompatError;
use crate::anthropic_compat::types::{
    ContentBlockDelta, MessagesResponse, MessagesResponseChunk, ResponseContentBlock, Usage,
};
use crate::openai_adapter::OpenAIAdapterError;
use crate::openai_adapter::types::ChatCompletionsResponseChunk;

use super::{finish_reason_map, map_id};

// ============================================================================
// 状态机
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq)]
enum BlockKind {
    None,
    Thinking,
    Text,
    ToolUse,
}

struct StreamState {
    block_kind: BlockKind,
    block_index: usize,
    model: String,
    message_id: String,
    input_tokens: u32,
    completion_tokens: Option<u32>,
    started: bool,
    finished: bool,
}

impl StreamState {
    const fn new() -> Self {
        Self {
            block_kind: BlockKind::None,
            block_index: 0,
            model: String::new(),
            message_id: String::new(),
            input_tokens: 0,
            completion_tokens: None,
            started: false,
            finished: false,
        }
    }

    fn start(&mut self, id: &str, model: &str) {
        self.message_id = map_id(id);
        model.clone_into(&mut self.model);
        self.started = true;
    }

    fn make_message_start(&self) -> MessagesResponseChunk {
        MessagesResponseChunk::MessageStart {
            message: MessagesResponse {
                id: self.message_id.clone(),
                ty: "message",
                role: "assistant",
                model: self.model.clone(),
                content: Vec::new(),
                stop_reason: None,
                stop_sequence: None,
                usage: Usage {
                    input_tokens: self.input_tokens,
                    output_tokens: 0,
                },
            },
        }
    }

    fn transition_to(&mut self, kind: BlockKind) -> Vec<MessagesResponseChunk> {
        let mut events = Vec::new();
        if self.block_kind != BlockKind::None {
            events.push(MessagesResponseChunk::ContentBlockStop {
                index: self.block_index,
            });
            self.block_index += 1;
        }
        self.block_kind = kind;
        events
    }

    fn handle_chunk(&mut self, chunk: &ChatCompletionsResponseChunk) -> Vec<MessagesResponseChunk> {
        let mut events = Vec::new();

        // 保活块 → 发送协议规定的 `ping` 事件
        //
        // 早期实现把它转成 thinking 增量的字面文本 "tool_calls..."，
        // 会直接出现在客户端的思考过程里（issue #76）。
        // `ping` 是 Anthropic 官方协议中的合法事件，客户端会忽略其内容。
        if chunk.id == "chatcmpl-keepalive" && self.started {
            events.push(MessagesResponseChunk::Ping);
            return events;
        }

        // role chunk → message_start（此时 chunk 已携带 prompt_tokens）
        if !self.started
            && let Some(choice) = chunk.choices.first()
            && choice.delta.role == Some("assistant")
        {
            self.start(&chunk.id, &chunk.model);
            if let Some(ref u) = chunk.usage {
                self.input_tokens = u.prompt_tokens;
            }
            events.push(self.make_message_start());
            return events;
        }

        // 优先提取 usage（可能独立 chunk 或与 finish 同 chunk）
        if let Some(ref u) = chunk.usage {
            self.completion_tokens = Some(u.completion_tokens);
        }

        let Some(choice) = chunk.choices.first() else {
            return events;
        };

        let delta = &choice.delta;

        // reasoning_content
        if let Some(ref text) = delta.reasoning_content
            && !text.is_empty()
        {
            if self.block_kind != BlockKind::Thinking {
                events.extend(self.transition_to(BlockKind::Thinking));
                events.push(MessagesResponseChunk::ContentBlockStart {
                    index: self.block_index,
                    content_block: ResponseContentBlock::Thinking {
                        thinking: String::new(),
                        signature: String::new(),
                    },
                });
            }
            events.push(MessagesResponseChunk::ContentBlockDelta {
                index: self.block_index,
                delta: ContentBlockDelta::Thinking {
                    thinking: text.clone(),
                },
            });
        }

        // content
        if let Some(ref text) = delta.content
            && !text.is_empty()
        {
            if self.block_kind != BlockKind::Text {
                events.extend(self.transition_to(BlockKind::Text));
                events.push(MessagesResponseChunk::ContentBlockStart {
                    index: self.block_index,
                    content_block: ResponseContentBlock::Text {
                        text: String::new(),
                    },
                });
            }
            events.push(MessagesResponseChunk::ContentBlockDelta {
                index: self.block_index,
                delta: ContentBlockDelta::Text { text: text.clone() },
            });
        }

        // tool_calls（一次性完整输出）
        if let Some(ref calls) = delta.tool_calls
            && !calls.is_empty()
        {
            events.extend(self.transition_to(BlockKind::ToolUse));
            for call in calls {
                let (name, partial_json) = call
                    .function
                    .as_ref()
                    .map(|func| (func.name.clone(), func.arguments.clone()))
                    .or_else(|| {
                        call.custom.as_ref().map(|custom| {
                            let json = serde_json::to_string(&custom.input)
                                .unwrap_or_else(|_| "{}".to_string());
                            (custom.name.clone(), json)
                        })
                    })
                    .unwrap_or_else(|| (String::new(), "{}".to_string()));
                events.push(MessagesResponseChunk::ContentBlockStart {
                    index: self.block_index,
                    content_block: ResponseContentBlock::ToolUse {
                        id: map_id(&call.id),
                        name: name.clone(),
                        input: serde_json::json!({}),
                    },
                });
                events.push(MessagesResponseChunk::ContentBlockDelta {
                    index: self.block_index,
                    delta: ContentBlockDelta::InputJson { partial_json },
                });
                events.push(MessagesResponseChunk::ContentBlockStop {
                    index: self.block_index,
                });
                self.block_index += 1;
            }
            self.block_kind = BlockKind::None;
        }

        // finish_reason
        if let Some(reason) = choice.finish_reason
            && !self.finished
        {
            self.finished = true;
            events.extend(self.transition_to(BlockKind::None));
            let stop_reason = finish_reason_map(reason);
            events.push(MessagesResponseChunk::MessageDelta {
                stop_reason: Some(stop_reason),
                stop_sequence: None,
                output_tokens: Some(self.completion_tokens.unwrap_or(0)),
            });
            events.push(MessagesResponseChunk::MessageStop);
        }

        events
    }
}

// ============================================================================
// AnthropicStream 转换器
// ============================================================================

pin_project! {
    struct AnthropicStream<S> {
        #[pin]
        inner: S,
        state: StreamState,
        pending_events: VecDeque<MessagesResponseChunk>,
    }
}

impl<S> AnthropicStream<S> {
    const fn new(inner: S) -> Self {
        Self {
            inner,
            state: StreamState::new(),
            pending_events: VecDeque::new(),
        }
    }
}

impl<S> Stream for AnthropicStream<S>
where
    S: Stream<Item = Result<ChatCompletionsResponseChunk, OpenAIAdapterError>>,
{
    type Item = Result<MessagesResponseChunk, AnthropicCompatError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut this = self.project();

        // 优先输出待处理事件
        if !this.pending_events.is_empty() {
            let event = this.pending_events.pop_front().expect("非空队列");
            return Poll::Ready(Some(Ok(event)));
        }

        loop {
            match this.inner.as_mut().poll_next(cx) {
                Poll::Ready(Some(Ok(chunk))) => {
                    trace!(target: "anthropic_compat::response::stream", "<<< {}",
                        serde_json::to_string(&chunk).unwrap_or_default());
                    let events = this.state.handle_chunk(&chunk);
                    this.pending_events.extend(events);
                    if !this.pending_events.is_empty() {
                        let event = this.pending_events.pop_front().expect("非空队列");
                        return Poll::Ready(Some(Ok(event)));
                    }
                }
                Poll::Ready(Some(Err(e))) => {
                    if !this.state.started {
                        return Poll::Ready(Some(Err(AnthropicCompatError::from(e))));
                    }
                    if !this.state.finished {
                        debug!(
                            target: "anthropic_compat::response::stream",
                            "上游流错误后补齐 Anthropic 收尾事件: {e}"
                        );
                        this.state.finished = true;
                        let mut events: Vec<MessagesResponseChunk> =
                            this.state.transition_to(BlockKind::None);
                        // 规范允许在流中途下发 `error` 事件，SDK 会据此抛出错误。
                        // 只补 message_stop 会让客户端把「上游中断」当成正常结束。
                        let (error_type, message) = match &e {
                            OpenAIAdapterError::Overloaded => {
                                ("overloaded_error", "上游限流，请稍后重试".to_string())
                            }
                            OpenAIAdapterError::BadRequest(msg) => {
                                ("invalid_request_error", msg.clone())
                            }
                            _ => ("api_error", e.to_string()),
                        };
                        events.push(MessagesResponseChunk::Error {
                            error_type,
                            message,
                        });
                        events.push(MessagesResponseChunk::MessageDelta {
                            stop_reason: None,
                            stop_sequence: None,
                            output_tokens: Some(this.state.completion_tokens.unwrap_or(0)),
                        });
                        events.push(MessagesResponseChunk::MessageStop);
                        this.pending_events.extend(events);
                    }
                    if !this.pending_events.is_empty() {
                        let event = this.pending_events.pop_front().expect("非空队列");
                        return Poll::Ready(Some(Ok(event)));
                    }
                    return Poll::Ready(None);
                }
                Poll::Ready(None) => {
                    debug!(target: "anthropic_compat::response::stream", "stream ended, started={}, finished={}", this.state.started, this.state.finished);
                    // 流结束但未收到 finish_reason：优雅关闭
                    if !this.state.finished && this.state.started {
                        this.state.finished = true;
                        let mut events: Vec<MessagesResponseChunk> =
                            this.state.transition_to(BlockKind::None);
                        events.push(MessagesResponseChunk::MessageDelta {
                            stop_reason: None,
                            stop_sequence: None,
                            output_tokens: Some(this.state.completion_tokens.unwrap_or(0)),
                        });
                        events.push(MessagesResponseChunk::MessageStop);
                        this.pending_events.extend(events);
                    }
                    if !this.pending_events.is_empty() {
                        let event = this.pending_events.pop_front().expect("非空队列");
                        return Poll::Ready(Some(Ok(event)));
                    }
                    return Poll::Ready(None);
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

// ============================================================================
// 公共入口
// ============================================================================

/// 将 ChatCompletionsResponseChunk 流映射为 MessagesResponseChunk 流
pub fn from_chat_completion_stream<S>(
    openai_stream: S,
) -> Pin<Box<dyn Stream<Item = Result<MessagesResponseChunk, AnthropicCompatError>> + Send>>
where
    S: Stream<Item = Result<ChatCompletionsResponseChunk, OpenAIAdapterError>> + Send + 'static,
{
    debug!(target: "anthropic_compat::response::stream", "starting streaming response mapping");
    Box::pin(AnthropicStream::new(openai_stream))
}

#[cfg(test)]
mod tests {
    use futures::StreamExt;

    use crate::anthropic_compat::types::{
        ContentBlockDelta, MessagesResponseChunk, ResponseContentBlock,
    };
    use crate::openai_adapter::OpenAIAdapterError;
    use crate::openai_adapter::types::{
        ChatCompletionsResponseChunk, ChunkChoice, Delta, FunctionCall, ToolCall, Usage,
    };

    fn role_chunk(model: &str, id: &str) -> ChatCompletionsResponseChunk {
        ChatCompletionsResponseChunk {
            id: id.to_string(),
            object: "chat.completion.chunk",
            created: 1000,
            model: model.to_string(),
            choices: vec![ChunkChoice {
                index: 0,
                delta: Delta {
                    role: Some("assistant"),
                    ..Default::default()
                },
                finish_reason: None,
                logprobs: None,
            }],
            usage: None,
            service_tier: None,
            system_fingerprint: None,
            obfuscation: None,
        }
    }

    fn content_chunk(content: &str) -> ChatCompletionsResponseChunk {
        ChatCompletionsResponseChunk {
            id: String::new(),
            object: "chat.completion.chunk",
            created: 1000,
            model: String::new(),
            choices: vec![ChunkChoice {
                index: 0,
                delta: Delta {
                    content: Some(content.to_string()),
                    ..Default::default()
                },
                finish_reason: None,
                logprobs: None,
            }],
            usage: None,
            service_tier: None,
            system_fingerprint: None,
            obfuscation: None,
        }
    }

    fn reasoning_chunk(text: &str) -> ChatCompletionsResponseChunk {
        ChatCompletionsResponseChunk {
            id: String::new(),
            object: "chat.completion.chunk",
            created: 1000,
            model: String::new(),
            choices: vec![ChunkChoice {
                index: 0,
                delta: Delta {
                    reasoning_content: Some(text.to_string()),
                    ..Default::default()
                },
                finish_reason: None,
                logprobs: None,
            }],
            usage: None,
            service_tier: None,
            system_fingerprint: None,
            obfuscation: None,
        }
    }

    fn tool_chunk(tool_calls: Vec<ToolCall>) -> ChatCompletionsResponseChunk {
        ChatCompletionsResponseChunk {
            id: String::new(),
            object: "chat.completion.chunk",
            created: 1000,
            model: String::new(),
            choices: vec![ChunkChoice {
                index: 0,
                delta: Delta {
                    tool_calls: Some(tool_calls),
                    ..Default::default()
                },
                finish_reason: None,
                logprobs: None,
            }],
            usage: None,
            service_tier: None,
            system_fingerprint: None,
            obfuscation: None,
        }
    }

    fn finish_chunk(reason: &'static str) -> ChatCompletionsResponseChunk {
        ChatCompletionsResponseChunk {
            id: String::new(),
            object: "chat.completion.chunk",
            created: 1000,
            model: String::new(),
            choices: vec![ChunkChoice {
                index: 0,
                delta: Delta::default(),
                finish_reason: Some(reason),
                logprobs: None,
            }],
            usage: None,
            service_tier: None,
            system_fingerprint: None,
            obfuscation: None,
        }
    }

    fn usage_chunk(prompt_tokens: u32, completion_tokens: u32) -> ChatCompletionsResponseChunk {
        ChatCompletionsResponseChunk {
            id: String::new(),
            object: "chat.completion.chunk",
            created: 1000,
            model: String::new(),
            choices: vec![],
            usage: Some(Usage {
                prompt_tokens,
                completion_tokens,
                total_tokens: prompt_tokens + completion_tokens,
                prompt_tokens_details: None,
                completion_tokens_details: None,
            }),
            service_tier: None,
            system_fingerprint: None,
            obfuscation: None,
        }
    }

    fn keepalive_chunk() -> ChatCompletionsResponseChunk {
        ChatCompletionsResponseChunk {
            id: "chatcmpl-keepalive".to_string(),
            object: "chat.completion.chunk",
            created: 1000,
            model: String::new(),
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
        }
    }

    async fn collect(chunks: Vec<ChatCompletionsResponseChunk>) -> Vec<MessagesResponseChunk> {
        let stream = futures::stream::iter(chunks.into_iter().map(Ok::<_, OpenAIAdapterError>));
        let mut anthropic = super::from_chat_completion_stream(stream);
        let mut events = Vec::new();
        while let Some(event) = anthropic.next().await {
            events.push(event.unwrap());
        }
        events
    }

    async fn collect_results(
        chunks: Vec<Result<ChatCompletionsResponseChunk, OpenAIAdapterError>>,
    ) -> Vec<Result<MessagesResponseChunk, crate::anthropic_compat::AnthropicCompatError>> {
        let stream = futures::stream::iter(chunks);
        let mut anthropic = super::from_chat_completion_stream(stream);
        let mut events = Vec::new();
        while let Some(event) = anthropic.next().await {
            events.push(event);
        }
        events
    }

    #[tokio::test]
    async fn text_only() {
        let events = collect(vec![
            role_chunk("deepseek-default", "chatcmpl-abc"),
            content_chunk("Hello"),
            finish_chunk("stop"),
        ])
        .await;
        assert!(!events.is_empty());
        // message_start
        assert_eq!(events[0].event_name(), "message_start");
        // content_block_start(text), content_block_delta, content_block_stop
        assert_eq!(events[1].event_name(), "content_block_start");
        assert_eq!(events[2].event_name(), "content_block_delta");
        assert_eq!(events[3].event_name(), "content_block_stop");
        // message_delta + message_stop
        assert_eq!(events[4].event_name(), "message_delta");
        assert_eq!(events[5].event_name(), "message_stop");
        assert_eq!(events.len(), 6);
    }

    #[tokio::test]
    async fn thinking_then_text() {
        let events = collect(vec![
            role_chunk("deepseek-expert", "chatcmpl-xyz"),
            reasoning_chunk("thinking..."),
            content_chunk("Answer."),
            finish_chunk("stop"),
        ])
        .await;
        // message_start
        assert_eq!(events[0].event_name(), "message_start");
        // content_block_start(thinking) + content_block_delta + content_block_stop
        assert_eq!(events[1].event_name(), "content_block_start");
        assert_eq!(events[2].event_name(), "content_block_delta");
        assert_eq!(events[3].event_name(), "content_block_stop");
        // content_block_start(text) + content_block_delta + content_block_stop
        assert_eq!(events[4].event_name(), "content_block_start");
        assert_eq!(events[5].event_name(), "content_block_delta");
        assert_eq!(events[6].event_name(), "content_block_stop");
        // message_delta + message_stop
        assert_eq!(events[7].event_name(), "message_delta");
        assert_eq!(events[8].event_name(), "message_stop");
        assert_eq!(events.len(), 9);
    }

    #[tokio::test]
    async fn tool_calls_only() {
        let calls = vec![ToolCall {
            id: "call_abc".to_string(),
            ty: "function".to_string(),
            function: Some(FunctionCall {
                name: "get_weather".to_string(),
                arguments: r#"{"city":"Beijing"}"#.to_string(),
            }),
            custom: None,
            index: 0,
        }];
        let events = collect(vec![
            role_chunk("deepseek-default", "chatcmpl-1"),
            tool_chunk(calls),
            finish_chunk("tool_calls"),
        ])
        .await;
        // message_start
        assert_eq!(events[0].event_name(), "message_start");
        // tool_use: content_block_start + input_json_delta + content_block_stop
        assert_eq!(events[1].event_name(), "content_block_start");
        assert_eq!(events[2].event_name(), "content_block_delta");
        assert_eq!(events[3].event_name(), "content_block_stop");
        // message_delta(tool_use) + message_stop
        assert_eq!(events[4].event_name(), "message_delta");
        assert_eq!(events[5].event_name(), "message_stop");
        assert_eq!(events.len(), 6);
        // verify tool_use content
        if let MessagesResponseChunk::ContentBlockStart {
            ref content_block, ..
        } = events[1]
        {
            assert!(matches!(
                content_block,
                ResponseContentBlock::ToolUse { .. }
            ));
        } else {
            panic!("expected ToolUse");
        }
        // check message_delta has tool_use stop_reason
        if let MessagesResponseChunk::MessageDelta {
            ref stop_reason, ..
        } = events[4]
        {
            assert_eq!(stop_reason.as_deref(), Some("tool_use"));
        } else {
            panic!("expected MessageDelta");
        }
    }

    #[tokio::test]
    async fn leading_text_then_tool_calls() {
        let calls = vec![ToolCall {
            id: "call_1".to_string(),
            ty: "function".to_string(),
            function: Some(FunctionCall {
                name: "search".to_string(),
                arguments: r#"{"q":"weather"}"#.to_string(),
            }),
            custom: None,
            index: 0,
        }];
        let events = collect(vec![
            role_chunk("deepseek-default", "chatcmpl-2"),
            content_chunk("Let me check."),
            tool_chunk(calls),
            finish_chunk("tool_calls"),
        ])
        .await;
        // message_start → text block → tool_use block → message_delta → message_stop
        assert_eq!(events[0].event_name(), "message_start");
        // text: start + delta + stop (stopped when transitioning to tool_use)
        assert_eq!(events[1].event_name(), "content_block_start");
        assert_eq!(events[2].event_name(), "content_block_delta");
        assert_eq!(events[3].event_name(), "content_block_stop");
        // tool_use: start + delta + stop
        assert_eq!(events[4].event_name(), "content_block_start");
        assert_eq!(events[5].event_name(), "content_block_delta");
        assert_eq!(events[6].event_name(), "content_block_stop");
        // message_delta + message_stop
        assert_eq!(events[7].event_name(), "message_delta");
        assert_eq!(events[8].event_name(), "message_stop");
        assert_eq!(events.len(), 9);
    }

    #[tokio::test]
    async fn multiple_tool_calls() {
        let calls = vec![
            ToolCall {
                id: "call_a".to_string(),
                ty: "function".to_string(),
                function: Some(FunctionCall {
                    name: "get_weather".to_string(),
                    arguments: r#"{"city":"Beijing"}"#.to_string(),
                }),
                custom: None,
                index: 0,
            },
            ToolCall {
                id: "call_b".to_string(),
                ty: "function".to_string(),
                function: Some(FunctionCall {
                    name: "get_time".to_string(),
                    arguments: r#"{"tz":"UTC"}"#.to_string(),
                }),
                custom: None,
                index: 1,
            },
        ];
        let events = collect(vec![
            role_chunk("deepseek-default", "chatcmpl-3"),
            tool_chunk(calls),
            finish_chunk("tool_calls"),
        ])
        .await;
        // message_start
        assert_eq!(events[0].event_name(), "message_start");
        // tool_use 1: start + delta + stop
        assert_eq!(events[1].event_name(), "content_block_start");
        assert_eq!(events[2].event_name(), "content_block_delta");
        assert_eq!(events[3].event_name(), "content_block_stop");
        // tool_use 2: start + delta + stop
        assert_eq!(events[4].event_name(), "content_block_start");
        assert_eq!(events[5].event_name(), "content_block_delta");
        assert_eq!(events[6].event_name(), "content_block_stop");
        // message_delta + message_stop
        assert_eq!(events[7].event_name(), "message_delta");
        assert_eq!(events[8].event_name(), "message_stop");
    }

    #[tokio::test]
    async fn keepalive_during_text() {
        let events = collect(vec![
            role_chunk("deepseek-default", "chatcmpl-4"),
            content_chunk("Hello"),
            keepalive_chunk(),
            content_chunk(" world"),
            finish_chunk("stop"),
        ])
        .await;
        // message_start
        assert_eq!(events[0].event_name(), "message_start");
        // text block start
        assert_eq!(events[1].event_name(), "content_block_start");
        // text delta
        assert_eq!(events[2].event_name(), "content_block_delta");
        // keepalive → ping（不打断当前文本块）
        assert_eq!(events[3].event_name(), "ping");
        // 后续文本继续落在同一个文本块里
        assert_eq!(events[4].event_name(), "content_block_delta");
        // finish: stop text + message_delta + message_stop
        assert_eq!(events[5].event_name(), "content_block_stop");
        assert_eq!(events[6].event_name(), "message_delta");
        assert_eq!(events[7].event_name(), "message_stop");
    }

    /// 回归（issue #76）：保活不得注入 `tool_calls...` 之类的假思考文本。
    #[tokio::test]
    async fn keepalive_emits_ping_not_fake_thinking() {
        let events = collect(vec![
            role_chunk("deepseek-default", "chatcmpl-5"),
            content_chunk("Hi"),
            keepalive_chunk(),
            finish_chunk("stop"),
        ])
        .await;

        // 整个事件序列里不得出现被伪造的思考文本
        for ev in &events {
            if let MessagesResponseChunk::ContentBlockDelta { delta, .. } = ev
                && let ContentBlockDelta::Thinking { thinking } = delta
            {
                assert!(
                    !thinking.contains("tool_calls"),
                    "保活不得注入假思考文本（issue #76），实际: {thinking:?}"
                );
            }
        }
        // 保活必须产生协议合法的 ping 事件
        assert!(
            events.iter().any(|e| e.event_name() == "ping"),
            "保活应产生 ping 事件"
        );
    }

    #[tokio::test]
    async fn usage_from_separate_chunk() {
        let events = collect(vec![
            role_chunk("deepseek-default", "chatcmpl-6"),
            content_chunk("Hi"),
            usage_chunk(10, 5),
            finish_chunk("stop"),
        ])
        .await;
        // message_delta should carry output_tokens from usage
        if let MessagesResponseChunk::MessageDelta {
            ref output_tokens, ..
        } = events[events.len() - 2]
        {
            assert_eq!(output_tokens.unwrap_or(0), 5);
        } else {
            panic!("expected MessageDelta");
        }
    }

    #[tokio::test]
    async fn finish_without_content() {
        let events = collect(vec![
            role_chunk("deepseek-default", "chatcmpl-7"),
            finish_chunk("stop"),
        ])
        .await;
        // message_start → message_delta → message_stop
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].event_name(), "message_start");
        assert_eq!(events[1].event_name(), "message_delta");
        assert_eq!(events[2].event_name(), "message_stop");
    }

    #[tokio::test]
    async fn stream_end_without_finish() {
        let events = collect(vec![
            role_chunk("deepseek-default", "chatcmpl-8"),
            content_chunk("Hi"),
        ])
        .await;
        // graceful shutdown: message_start → text block → ...stop?
        // stream end without finish_reason: transition_to(None) + message_delta + message_stop
        assert!(events.last().is_some());
        assert_eq!(events.last().unwrap().event_name(), "message_stop");
        // should have message_delta before message_stop
        let delta_idx = events.len() - 2;
        assert_eq!(events[delta_idx].event_name(), "message_delta");
        // stop_reason should be None for graceful shutdown
        if let MessagesResponseChunk::MessageDelta { stop_reason, .. } = &events[delta_idx] {
            assert_eq!(stop_reason, &None);
        }
    }

    #[tokio::test]
    async fn upstream_error_after_start_emits_error_then_stop() {
        let events = collect_results(vec![
            Ok(role_chunk("deepseek-default", "chatcmpl-err")),
            Ok(content_chunk("Hi")),
            Err(OpenAIAdapterError::Internal("upstream interrupted".into())),
        ])
        .await;

        assert!(events.iter().all(Result::is_ok));
        let events: Vec<_> = events.into_iter().map(Result::unwrap).collect();
        assert_eq!(events.last().unwrap().event_name(), "message_stop");
        assert_eq!(events[events.len() - 2].event_name(), "message_delta");
        // 规范里的 `error` 事件：SDK 会抛出错误，而不是把截断当成正常结束
        assert_eq!(events[events.len() - 3].event_name(), "error");
    }

    #[tokio::test]
    async fn message_id_mapped() {
        let events = collect(vec![
            role_chunk("deepseek-default", "chatcmpl-a1b2c3"),
            finish_chunk("stop"),
        ])
        .await;
        if let MessagesResponseChunk::MessageStart { ref message } = events[0] {
            assert_eq!(message.id, "msg_a1b2c3");
        } else {
            panic!("expected MessageStart");
        }
    }

    #[tokio::test]
    async fn tool_use_id_mapped() {
        let calls = vec![ToolCall {
            id: "call_xyz".to_string(),
            ty: "function".to_string(),
            function: Some(FunctionCall {
                name: "f".to_string(),
                arguments: "{}".to_string(),
            }),
            custom: None,
            index: 0,
        }];
        let events = collect(vec![
            role_chunk("deepseek-default", "chatcmpl-9"),
            tool_chunk(calls),
            finish_chunk("tool_calls"),
        ])
        .await;
        if let MessagesResponseChunk::ContentBlockStart {
            ref content_block, ..
        } = events[1]
        {
            if let ResponseContentBlock::ToolUse { id, .. } = content_block {
                assert_eq!(id, "toolu_xyz");
            } else {
                panic!("expected ToolUse");
            }
        }
    }
}
