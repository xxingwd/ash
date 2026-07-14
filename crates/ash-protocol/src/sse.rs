use ash_core::ProtocolError;
use futures::StreamExt;
use reqwest::RequestBuilder;
use reqwest_eventsource::{retry::Never, Event, RequestBuilderExt};

use crate::{ProtocolStream, StreamItem};

pub(crate) trait Decoder: Send + 'static {
    fn decode(&mut self, data: &str) -> Result<Vec<StreamItem>, ProtocolError>;
    fn is_done(&self) -> bool;
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
                    for item in decoder.decode(&message.data)? {
                        yield item;
                    }
                    if decoder.is_done() {
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
