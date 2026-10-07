//! OpenAI Chunk 生成器 —— 将 StreamEvent 映射为 ChatCompletionsResponseChunk

use std::pin::Pin;
use std::task::{Context, Poll};

use futures::Stream;
use pin_project_lite::pin_project;

use log::{trace, warn};

use ds_core::StreamEvent;

use crate::openai_adapter::OpenAIAdapterError;
use crate::openai_adapter::types::{ChatCompletionsResponseChunk, ChunkChoice, Delta, Usage};

use super::{next_chatcmpl_id, now_secs};

const fn make_usage(prompt_tokens: u32, completion_tokens: u32) -> Usage {
    Usage {
        prompt_tokens,
        completion_tokens,
        total_tokens: prompt_tokens + completion_tokens,
        prompt_tokens_details: None,
        completion_tokens_details: None,
    }
}

pub(crate) fn make_chunk(
    model: &str,
    delta: Delta,
    finish: Option<&'static str>,
) -> ChatCompletionsResponseChunk {
    ChatCompletionsResponseChunk {
        id: next_chatcmpl_id(),
        object: "chat.completion.chunk",
        created: now_secs(),
        model: model.to_string(),
        choices: vec![ChunkChoice {
            index: 0,
            delta,
            finish_reason: finish,
            logprobs: None,
        }],
        usage: None,
        service_tier: None,
        system_fingerprint: None,
        obfuscation: None,
    }
}

pin_project! {
    // 将 StreamEvent 增量事件映射为 OpenAI ChatCompletionsResponseChunk 的流转换器
    pub struct ConverterStream<S> {
        #[pin]
        inner: S,
        model: String,
        include_usage: bool,
        include_obfuscation: bool,
        prompt_tokens: u32,
        finished: bool,
        role_sent: bool,
    }
}

impl<S> ConverterStream<S> {
    pub const fn new(
        inner: S,
        model: String,
        include_usage: bool,
        include_obfuscation: bool,
        prompt_tokens: u32,
    ) -> Self {
        Self {
            inner,
            model,
            include_usage,
            include_obfuscation,
            prompt_tokens,
            finished: false,
            role_sent: false,
        }
    }
}

impl<S> Stream for ConverterStream<S>
where
    S: Stream<Item = Result<StreamEvent, OpenAIAdapterError>>,
{
    type Item = Result<ChatCompletionsResponseChunk, OpenAIAdapterError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut this = self.project();

        loop {
            match this.inner.as_mut().poll_next(cx) {
                Poll::Ready(Some(Ok(event))) => match event {
                    StreamEvent::Meta { .. } => {
                        trace!(target: "adapter", ">>> conv: role=assistant");
                        *this.role_sent = true;
                        return Poll::Ready(Some(Ok(ChatCompletionsResponseChunk {
                            usage: Some(make_usage(*this.prompt_tokens, 0)),
                            ..make_chunk(
                                this.model,
                                Delta {
                                    role: Some("assistant"),
                                    ..Default::default()
                                },
                                None,
                            )
                        })));
                    }
                    StreamEvent::ThinkStart => {
                        // 转换器不需要对 ThinkStart 做特殊处理
                        continue;
                    }
                    StreamEvent::ThinkDelta { content } => {
                        trace!(target: "adapter", ">>> conv: thinking len={}", content.len());
                        return Poll::Ready(Some(Ok(make_chunk(
                            this.model,
                            Delta {
                                reasoning_content: Some(content),
                                ..Default::default()
                            },
                            None,
                        ))));
                    }
                    StreamEvent::ContentStart => {
                        continue;
                    }
                    StreamEvent::ContentDelta { content } => {
                        trace!(target: "adapter", ">>> conv: content delta len={}", content.len());
                        return Poll::Ready(Some(Ok(make_chunk(
                            this.model,
                            Delta {
                                content: Some(content),
                                ..Default::default()
                            },
                            None,
                        ))));
                    }
                    StreamEvent::Done {
                        finish_reason,
                        accumulated_token_usage,
                    } => {
                        trace!(target: "adapter", ">>> conv: finish={finish_reason:?}");
                        *this.finished = true;
                        // 采用上游给出的结束原因（如 length），未知/缺省才退化为 stop；
                        // 硬编码 "stop" 会把「被截断」伪装成「正常结束」
                        let finish = match finish_reason.as_deref() {
                            Some("length") => "length",
                            Some("tool_calls") => "tool_calls",
                            _ => "stop",
                        };
                        let mut chunk = make_chunk(this.model, Delta::default(), Some(finish));
                        if *this.include_usage
                            && let Some(u) = accumulated_token_usage
                        {
                            chunk.usage = Some(make_usage(*this.prompt_tokens, u));
                        }
                        return Poll::Ready(Some(Ok(chunk)));
                    }
                },
                Poll::Ready(Some(Err(e))) => return Poll::Ready(Some(Err(e))),
                Poll::Ready(None) => {
                    if !*this.finished {
                        warn!(target: "adapter", "converter stream ended early: model={}", this.model);
                    }
                    return Poll::Ready(None);
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}
