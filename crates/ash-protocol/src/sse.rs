use ash_core::{ModelEvent, ModelStream, ProtocolError, StopReason};
use eventsource_stream::Eventsource;
use futures::StreamExt;
use reqwest::RequestBuilder;

pub enum DecodeResult {
    Continue(Vec<ModelEvent>),
    Close(Vec<ModelEvent>),
}

pub trait Decoder: Send + 'static {
    fn decode(&mut self, data: &str) -> Result<DecodeResult, ProtocolError>;

    /// Consume all decoder state after EOF or a provider-specific wire close.
    fn finalize(self) -> Result<(Vec<ModelEvent>, StopReason), ProtocolError>;
}

#[derive(Default)]
struct StreamEventCounts {
    frames: usize,
    text_deltas: usize,
    reasoning_deltas: usize,
    tool_calls: usize,
    usage_reports: usize,
    stop_events: usize,
}

impl StreamEventCounts {
    fn observe(&mut self, item: &ModelEvent) {
        match item {
            ModelEvent::Text(_) => self.text_deltas += 1,
            ModelEvent::Reasoning(_) => self.reasoning_deltas += 1,
            ModelEvent::ToolCall { .. } => self.tool_calls += 1,
            ModelEvent::Usage(_) => self.usage_reports += 1,
            ModelEvent::Stop(_) => self.stop_events += 1,
        }
    }
}

pub fn stream<D>(request: RequestBuilder, mut decoder: D) -> ModelStream
where
    D: Decoder,
{
    Box::pin(async_stream::try_stream! {
        let response = request
            .send()
            .await
            .map_err(|error| ProtocolError::Request(error.to_string()))?;
        let status = response.status();
        if !status.is_success() {
            Err(map_status(status))?;
        }
        let mut source = response.bytes_stream().eventsource();
        let mut wire_closed = false;
        let mut counts = StreamEventCounts::default();
        while let Some(event) = source.next().await {
            let event = event.map_err(|error| ProtocolError::Request(error.to_string()))?;
            counts.frames += 1;
            let result = decoder.decode(&event.data).map_err(|error| {
                tracing::warn!(%error, data = %event.data, "sse decode failed");
                error
            })?;
            let (items, closes_wire) = match result {
                DecodeResult::Continue(items) => (items, false),
                DecodeResult::Close(items) => (items, true),
            };
            tracing::debug!(closes_wire, data = %event.data, "sse event");
            for item in items {
                validate_nonterminal(&item)?;
                log_model_event(&item);
                counts.observe(&item);
                yield item;
            }
            if closes_wire {
                wire_closed = true;
                break;
            }
        }

        let (items, stop) = decoder.finalize()?;
        let provider_terminated = stop != StopReason::Truncated;
        for item in items {
            validate_nonterminal(&item)?;
            log_model_event(&item);
            counts.observe(&item);
            yield item;
        }
        let stop = ModelEvent::Stop(stop);
        log_model_event(&stop);
        counts.observe(&stop);
        yield stop;

        tracing::debug!(
            frames = counts.frames,
            text_deltas = counts.text_deltas,
            reasoning_deltas = counts.reasoning_deltas,
            tool_calls = counts.tool_calls,
            usage_reports = counts.usage_reports,
            stop_events = counts.stop_events,
            provider_terminated,
            wire_closed,
            "sse stream summary"
        );
    })
}

/// Log the non-streaming model events from the wire. Text and reasoning
/// deltas are already visible in the raw SSE data and the agent event log,
/// so only the structural events (tool calls, usage, stop) are logged here
/// to keep the stream readable.
fn log_model_event(item: &ModelEvent) {
    match item {
        ModelEvent::Text(_) | ModelEvent::Reasoning(_) => {}
        other => tracing::debug!(?other, "model event"),
    }
}

/// Only the shared stream boundary may emit the terminal `ModelEvent::Stop`.
fn validate_nonterminal(item: &ModelEvent) -> Result<(), ProtocolError> {
    if matches!(item, ModelEvent::Stop(_)) {
        return Err(ProtocolError::InvalidResponse(
            "protocol decoder emitted a stop event before finalization".into(),
        ));
    }
    Ok(())
}

fn map_status(status: reqwest::StatusCode) -> ProtocolError {
    match status {
        reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN => ProtocolError::Auth,
        reqwest::StatusCode::TOO_MANY_REQUESTS => ProtocolError::RateLimited,
        _ => ProtocolError::Upstream {
            status: status.as_u16(),
            message: "request rejected before the event stream opened".into(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ash_core::{ModelEvent, StopReason};
    use tokio::io::AsyncWriteExt;

    /// Serve a single fixed-length SSE body over a raw TCP connection (then
    /// close it) and run `sse::stream` against it with the chat-completions
    /// decoder. This exercises the real EOF path, including a truncated
    /// stream that ends without `[DONE]` or `finish_reason`.
    async fn run_completions_stream(body: &str) -> Vec<ModelEvent> {
        run_stream(
            body,
            "/v1/chat/completions",
            crate::completions::CompletionsDecoder::default(),
        )
        .await
    }

    async fn run_responses_stream(body: &str) -> Vec<ModelEvent> {
        run_stream(
            body,
            "/v1/responses",
            crate::responses::ResponsesDecoder::default(),
        )
        .await
    }

    async fn run_anthropic_stream(body: &str) -> Vec<ModelEvent> {
        run_stream(
            body,
            "/v1/messages",
            crate::anthropic::AnthropicDecoder::default(),
        )
        .await
    }

    async fn run_stream<D: Decoder>(body: &str, path: &str, decoder: D) -> Vec<ModelEvent> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let body = body.to_string();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                 Content-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.shutdown().await.unwrap();
        });
        let request = reqwest::Client::new()
            .post(format!("http://{addr}{path}"))
            .bearer_auth("test-key")
            .json(&serde_json::json!({"model": "test", "stream": true}));
        let mut stream = super::stream(request, decoder);
        let mut events = Vec::new();
        while let Some(item) = stream.next().await {
            events.push(item.unwrap());
        }
        server.await.unwrap();
        events
    }

    #[tokio::test]
    async fn eof_without_terminal_marker_reports_truncated() {
        let events =
            run_completions_stream("data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n")
                .await;
        assert_eq!(
            events,
            vec![
                ModelEvent::Text("hi".into()),
                ModelEvent::Stop(StopReason::Truncated),
            ]
        );
    }

    #[tokio::test]
    async fn finish_reason_is_a_clean_stop() {
        let events = run_completions_stream(
            "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"},\"finish_reason\":\"stop\"}]}\n\n",
        )
        .await;
        assert_eq!(
            events,
            vec![
                ModelEvent::Text("hi".into()),
                ModelEvent::Stop(StopReason::EndTurn),
            ]
        );
    }

    #[tokio::test]
    async fn done_marker_without_finish_reason_is_truncated() {
        let events = run_completions_stream(
            "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n",
        )
        .await;
        assert_eq!(
            events,
            vec![
                ModelEvent::Text("hi".into()),
                ModelEvent::Stop(StopReason::Truncated),
            ]
        );
    }

    #[tokio::test]
    async fn completion_usage_after_finish_reason_precedes_the_only_stop() {
        let events = run_completions_stream(
            "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"},\"finish_reason\":\"stop\"}]}\n\n\
             data: {\"choices\":[],\"usage\":{\"prompt_tokens\":12,\"completion_tokens\":3}}\n\n\
             data: [DONE]\n\n",
        )
        .await;

        assert_eq!(
            events,
            vec![
                ModelEvent::Text("hi".into()),
                ModelEvent::Usage(ash_core::Usage {
                    input_tokens: 12,
                    output_tokens: 3,
                    generation_ms: 0,
                    estimated: false,
                }),
                ModelEvent::Stop(StopReason::EndTurn),
            ]
        );
    }

    #[tokio::test]
    async fn repeated_finish_reason_with_usage_is_accepted() {
        let events = run_completions_stream(
            "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"},\"finish_reason\":\"stop\"}]}\n\n\
             data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":12,\"completion_tokens\":3}}\n\n\
             data: [DONE]\n\n\
             data: {\"choices\":[],\"cost\":\"0\"}\n\n",
        )
        .await;

        assert_eq!(
            events,
            vec![
                ModelEvent::Text("hi".into()),
                ModelEvent::Usage(ash_core::Usage {
                    input_tokens: 12,
                    output_tokens: 3,
                    generation_ms: 0,
                    estimated: false,
                }),
                ModelEvent::Stop(StopReason::EndTurn),
            ]
        );
    }

    #[tokio::test]
    async fn content_after_finish_reason_is_accumulated_and_last_reason_wins() {
        let events = run_completions_stream(
            "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"},\"finish_reason\":\"stop\"}]}\n\n\
             data: {\"choices\":[{\"delta\":{\"content\":\" there\"},\"finish_reason\":\"length\"}]}\n\n\
             data: [DONE]\n\n",
        )
        .await;

        assert_eq!(
            events,
            vec![
                ModelEvent::Text("hi".into()),
                ModelEvent::Text(" there".into()),
                ModelEvent::Stop(StopReason::MaxTokens),
            ]
        );
    }

    #[tokio::test]
    async fn responses_done_without_completed_is_truncated() {
        let events = run_responses_stream(
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n\
             data: [DONE]\n\n",
        )
        .await;

        assert_eq!(
            events,
            vec![
                ModelEvent::Text("hi".into()),
                ModelEvent::Stop(StopReason::Truncated),
            ]
        );
    }

    #[tokio::test]
    async fn responses_completed_produces_one_shared_stop() {
        let events = run_responses_stream(
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n\
             data: {\"type\":\"response.completed\",\"response\":{}}\n\n",
        )
        .await;

        assert_eq!(
            events,
            vec![
                ModelEvent::Text("hi".into()),
                ModelEvent::Stop(StopReason::EndTurn),
            ]
        );
    }

    #[tokio::test]
    async fn anthropic_requires_message_stop_for_clean_completion() {
        let without_message_stop = run_anthropic_stream(
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n",
        )
        .await;
        let with_message_stop = run_anthropic_stream(
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n\
             data: {\"type\":\"message_stop\"}\n\n",
        )
        .await;

        assert_eq!(
            without_message_stop,
            vec![ModelEvent::Stop(StopReason::Truncated)]
        );
        assert_eq!(
            with_message_stop,
            vec![ModelEvent::Stop(StopReason::EndTurn)]
        );
    }
}
