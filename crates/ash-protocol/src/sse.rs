use ash_core::ProtocolError;
use futures::StreamExt;
use reqwest::RequestBuilder;
use reqwest_eventsource::{retry::Never, Event, RequestBuilderExt};

use crate::{ProtocolStream, StreamItem};

pub(crate) enum DecodeResult {
    Continue(Vec<StreamItem>),
    Finished(Vec<StreamItem>),
}

impl DecodeResult {
    pub(crate) fn continuing(items: Vec<StreamItem>) -> Self {
        Self::Continue(items)
    }

    pub(crate) fn finished(items: Vec<StreamItem>) -> Self {
        Self::Finished(items)
    }

    fn into_parts(self) -> (Vec<StreamItem>, bool) {
        match self {
            Self::Continue(items) => (items, false),
            Self::Finished(items) => (items, true),
        }
    }

    #[cfg(test)]
    pub(crate) fn into_items(self) -> Vec<StreamItem> {
        self.into_parts().0
    }
}

pub(crate) trait Decoder: Send + 'static {
    fn decode(&mut self, data: &str) -> Result<DecodeResult, ProtocolError>;
}

pub(crate) fn stream<D>(
    request: RequestBuilder,
    mut decoder: D,
) -> Result<ProtocolStream, ProtocolError>
where
    D: Decoder,
{
    let mut source = request
        .eventsource()
        .map_err(|error| ProtocolError::Request(error.to_string()))?;
    source.set_retry_policy(Box::new(Never));

    Ok(Box::pin(async_stream::try_stream! {
        while let Some(event) = source.next().await {
            match event {
                Ok(Event::Open) => {}
                Ok(Event::Message(message)) => {
                    let (items, finished) = decoder.decode(&message.data)?.into_parts();
                    for item in items {
                        yield item;
                    }
                    if finished {
                        source.close();
                        break;
                    }
                }
                Err(error) => {
                    source.close();
                    Err(map_error(error))?;
                }
            }
        }
    }))
}

fn map_error(error: reqwest_eventsource::Error) -> ProtocolError {
    match error {
        reqwest_eventsource::Error::InvalidStatusCode(status, _)
            if status.as_u16() == 401 || status.as_u16() == 403 =>
        {
            ProtocolError::Auth
        }
        reqwest_eventsource::Error::InvalidStatusCode(status, _) if status.as_u16() == 429 => {
            ProtocolError::RateLimited { retry_after: None }
        }
        reqwest_eventsource::Error::InvalidStatusCode(status, _) => ProtocolError::Upstream {
            status: status.as_u16(),
            message: "request rejected before the event stream opened".into(),
        },
        other => ProtocolError::Request(other.to_string()),
    }
}
