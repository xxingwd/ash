use ash_core::{ModelStream, ModelStreamEvent, ProtocolError};
use eventsource_stream::Eventsource;
use futures::StreamExt;
use reqwest::RequestBuilder;

pub(crate) enum DecodeResult {
    Continue(Vec<ModelStreamEvent>),
    Finished(Vec<ModelStreamEvent>),
}

impl DecodeResult {
    pub(crate) fn continuing(items: Vec<ModelStreamEvent>) -> Self {
        Self::Continue(items)
    }

    pub(crate) fn finished(items: Vec<ModelStreamEvent>) -> Self {
        Self::Finished(items)
    }

    fn into_parts(self) -> (Vec<ModelStreamEvent>, bool) {
        match self {
            Self::Continue(items) => (items, false),
            Self::Finished(items) => (items, true),
        }
    }

    #[cfg(test)]
    pub(crate) fn into_items(self) -> Vec<ModelStreamEvent> {
        self.into_parts().0
    }
}

pub(crate) trait Decoder: Send + 'static {
    fn decode(&mut self, data: &str) -> Result<DecodeResult, ProtocolError>;
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
        while let Some(event) = source.next().await {
            match event {
                Ok(event) => {
                    let (items, finished) = decoder.decode(&event.data)?.into_parts();
                    for item in items {
                        yield item;
                    }
                    if finished {
                        break;
                    }
                }
                Err(error) => {
                    Err(ProtocolError::Request(error.to_string()))?;
                }
            }
        }
    }))
}

fn map_status(status: reqwest::StatusCode) -> ProtocolError {
    match status.as_u16() {
        401 | 403 => ProtocolError::Auth,
        429 => ProtocolError::RateLimited { retry_after: None },
        _ => ProtocolError::Upstream {
            status: status.as_u16(),
            message: "request rejected before the event stream opened".into(),
        },
    }
}
