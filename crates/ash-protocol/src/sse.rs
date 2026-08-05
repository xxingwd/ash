use ash_core::{ModelEvent, ModelStream, ProtocolError, StopReason};
use eventsource_stream::Eventsource;
use futures::StreamExt;
use reqwest::RequestBuilder;

pub(crate) enum DecodeResult {
    Continue(Vec<ModelEvent>),
    /// A provider-level terminal marker was observed, but more wire events may
    /// follow (for example Chat Completions sends usage after `finish_reason`).
    Terminal(Vec<ModelEvent>),
    /// A provider-level terminal marker was observed and the wire can stop.
    Finished(Vec<ModelEvent>),
    /// The wire ended without a provider-level terminal marker.
    WireDone(Vec<ModelEvent>),
}

impl DecodeResult {
    pub(crate) fn continuing(items: Vec<ModelEvent>) -> Self {
        Self::Continue(items)
    }

    pub(crate) fn terminal(items: Vec<ModelEvent>) -> Self {
        Self::Terminal(items)
    }

    pub(crate) fn finished(items: Vec<ModelEvent>) -> Self {
        Self::Finished(items)
    }

    pub(crate) fn wire_done(items: Vec<ModelEvent>) -> Self {
        Self::WireDone(items)
    }

    fn into_parts(self) -> (Vec<ModelEvent>, bool, bool) {
        match self {
            Self::Continue(items) => (items, false, false),
            Self::Terminal(items) => (items, true, false),
            Self::Finished(items) => (items, true, true),
            Self::WireDone(items) => (items, false, true),
        }
    }

    #[cfg(test)]
    pub(crate) fn into_items(self) -> Vec<ModelEvent> {
        self.into_parts().0
    }
}

pub(crate) trait Decoder: Send + 'static {
    fn decode(&mut self, data: &str) -> Result<DecodeResult, ProtocolError>;

    /// Emit events buffered until the wire closes. Most protocols emit their
    /// stop event directly; Chat Completions delays it so trailing usage stays
    /// before the terminal `ModelEvent::Stop`.
    fn finish(&mut self) -> Result<Vec<ModelEvent>, ProtocolError> {
        Ok(Vec::new())
    }
}

pub(crate) fn stream<D>(
    request: RequestBuilder,
    mut decoder: D,
) -> Result<ModelStream, ProtocolError>
where
    D: Decoder,
{
    Ok(Box::pin(async_stream::try_stream! {
        let response = request
            .send()
            .await
            .map_err(|error| ProtocolError::Request(error.to_string()))?;
        let status = response.status();
        if !status.is_success() {
            Err(map_status(status))?;
        }
        let mut source = response.bytes_stream().eventsource();
        // Whether the decoder observed a protocol-level terminal marker
        // (`finish_reason`, `message_stop`, or `response.completed`). Wire-only
        // delimiters such as `[DONE]` do not make an incomplete response clean.
        // A stream that ends without one is a truncated response, not a
        // normal stop.
        let mut terminated = false;
        let mut saw_stop = false;
        while let Some(event) = source.next().await {
            match event {
                Ok(event) => {
                    let (items, terminal, wire_done) = decoder.decode(&event.data)?.into_parts();
                    terminated |= terminal;
                    for item in items {
                        if matches!(item, ModelEvent::Stop(_)) {
                            if saw_stop {
                                Err(ProtocolError::InvalidResponse(
                                    "provider stream emitted more than one stop event".into(),
                                ))?;
                            }
                            saw_stop = true;
                        }
                        yield item;
                    }
                    if wire_done {
                        break;
                    }
                }
                Err(error) => {
                    Err(ProtocolError::Request(error.to_string()))?;
                }
            }
        }
        if terminated {
            for item in decoder.finish()? {
                if matches!(item, ModelEvent::Stop(_)) {
                    if saw_stop {
                        Err(ProtocolError::InvalidResponse(
                            "provider stream emitted more than one stop event".into(),
                        ))?;
                    }
                    saw_stop = true;
                }
                yield item;
            }
            if !saw_stop {
                Err(ProtocolError::InvalidResponse(
                    "provider terminal marker did not produce a stop event".into(),
                ))?;
            }
        } else {
            // EOF (or a wire-only marker) without a provider terminal means
            // the response was truncated, even if partial output was emitted.
            yield ModelEvent::Stop(StopReason::Truncated);
        }
    }))
}

fn map_status(status: reqwest::StatusCode) -> ProtocolError {
    match status {
        reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN => ProtocolError::Auth,
        reqwest::StatusCode::TOO_MANY_REQUESTS => ProtocolError::RateLimited { retry_after: None },
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
        let mut stream = super::stream(request, decoder).unwrap();
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
}
