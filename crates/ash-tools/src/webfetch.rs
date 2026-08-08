use std::{sync::Arc, time::Duration};

use ash_core::{define_tool, CancellationToken, Tool, ToolError};
use htmd::HtmlToMarkdown;
use reqwest::{header, Client, Response, Url};
use schemars::JsonSchema;
use serde::Deserialize;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_RESPONSE_BYTES: usize = 5 * 1024 * 1024;
const ASH_USER_AGENT: &str = concat!("ash/", env!("CARGO_PKG_VERSION"));
const ACCEPT: &str =
    "text/markdown;q=1.0, text/x-markdown;q=0.9, text/plain;q=0.8, text/html;q=0.7, application/xhtml+xml;q=0.7, */*;q=0.1";

#[derive(Deserialize, JsonSchema)]
struct WebFetchArgs {
    /// HTTP or HTTPS URL to fetch
    url: String,
    /// Request timeout in seconds; defaults to 30 and cannot exceed 120
    timeout: Option<f64>,
}

pub fn tool() -> Result<Arc<dyn Tool>, ToolError> {
    let client = Client::new();
    define_tool(
        "webfetch",
        "Fetch an HTTP or HTTPS URL. HTML is converted to Markdown; Markdown and other textual responses are returned as text. Responses are limited to 5MB.",
        move |ctx, args: WebFetchArgs| {
            let client = client.clone();
            let cancellation = ctx.cancellation;
            let deadline = ctx.deadline;
            async move {
                crate::path::ensure_running(&cancellation, deadline)?;
                let url = parse_url(&args.url)?;
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                let timeout = parse_timeout(args.timeout, remaining)?;
                let request = async { fetch(&client, url, &cancellation).await };
                tokio::select! {
                    result = tokio::time::timeout(timeout, request) => {
                        result.map_err(|_| ToolError::Timeout(timeout))?
                    }
                    _ = cancellation.cancelled() => Err(ToolError::Cancelled),
                }
            }
        },
    )
}

fn parse_url(input: &str) -> Result<Url, ToolError> {
    let url =
        Url::parse(input).map_err(|error| ToolError::Execution(format!("invalid URL: {error}")))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(ToolError::Execution(
            "URL must use http:// or https://".into(),
        ));
    }
    Ok(url)
}

fn parse_timeout(seconds: Option<f64>, framework_limit: Duration) -> Result<Duration, ToolError> {
    let requested = match seconds {
        Some(seconds) => crate::timeout::parse_positive_seconds(seconds)?,
        None => DEFAULT_TIMEOUT,
    };
    if requested > MAX_TIMEOUT {
        return Err(ToolError::Execution(format!(
            "timeout cannot exceed {} seconds",
            MAX_TIMEOUT.as_secs()
        )));
    }
    Ok(requested.min(framework_limit))
}

async fn fetch(
    client: &Client,
    url: Url,
    cancellation: &CancellationToken,
) -> Result<String, ToolError> {
    let response = send(client, url).await?;

    if !response.status().is_success() {
        return Err(ToolError::Execution(format!(
            "request to {} failed with HTTP {}",
            response.url(),
            response.status()
        )));
    }

    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if !is_textual_content_type(&content_type) {
        return Err(ToolError::Execution(format!(
            "unsupported response content type: {content_type}"
        )));
    }

    let body = collect_body(response, cancellation).await?;
    let text = String::from_utf8_lossy(&body).into_owned();
    if is_html(&content_type, &text) {
        return html_to_markdown(text, cancellation).await;
    }
    Ok(nonempty(text))
}

async fn send(client: &Client, url: Url) -> Result<Response, ToolError> {
    client
        .get(url)
        .header(header::USER_AGENT, ASH_USER_AGENT)
        .header(header::ACCEPT, ACCEPT)
        .header(header::ACCEPT_LANGUAGE, "en-US,en;q=0.9")
        .send()
        .await
        .map_err(|error| ToolError::Execution(format!("request failed: {error}")))
}

async fn collect_body(
    mut response: Response,
    cancellation: &CancellationToken,
) -> Result<Vec<u8>, ToolError> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
    {
        return Err(response_too_large());
    }

    let capacity = response
        .content_length()
        .and_then(|length| usize::try_from(length).ok())
        .unwrap_or(64 * 1024)
        .min(MAX_RESPONSE_BYTES);
    let mut body = Vec::with_capacity(capacity);
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| ToolError::Execution(format!("cannot read response: {error}")))?
    {
        if cancellation.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            return Err(response_too_large());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn response_too_large() -> ToolError {
    ToolError::Execution(format!(
        "response exceeds the {}MB limit",
        MAX_RESPONSE_BYTES / (1024 * 1024)
    ))
}

fn is_textual_content_type(content_type: &str) -> bool {
    content_type.is_empty()
        || content_type.starts_with("text/")
        || content_type == "application/json"
        || content_type.ends_with("+json")
        || content_type == "application/xml"
        || content_type.ends_with("+xml")
        || matches!(
            content_type,
            "application/javascript" | "application/x-javascript"
        )
}

fn is_html(content_type: &str, text: &str) -> bool {
    if matches!(content_type, "text/html" | "application/xhtml+xml") {
        return true;
    }
    if !content_type.is_empty() {
        return false;
    }
    let prefix = text
        .trim_start()
        .chars()
        .take(32)
        .collect::<String>()
        .to_ascii_lowercase();
    prefix.starts_with("<!doctype html") || prefix.starts_with("<html")
}

async fn html_to_markdown(
    html: String,
    cancellation: &CancellationToken,
) -> Result<String, ToolError> {
    if cancellation.is_cancelled() {
        return Err(ToolError::Cancelled);
    }
    tokio::task::spawn_blocking(move || {
        HtmlToMarkdown::builder()
            .skip_tags(vec![
                "script", "style", "meta", "link", "noscript", "iframe", "object", "embed",
                "template", "canvas", "svg",
            ])
            .build()
            .convert(&html)
    })
    .await
    .map_err(|error| ToolError::Execution(format!("HTML conversion task failed: {error}")))?
    .map(nonempty)
    .map_err(|error| ToolError::Execution(format!("cannot convert HTML to Markdown: {error}")))
}

fn nonempty(content: String) -> String {
    if content.trim().is_empty() {
        "(empty response)".into()
    } else {
        content
    }
}

#[cfg(test)]
mod tests {
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        sync::mpsc,
    };

    use super::*;

    #[test]
    fn validates_urls_and_timeouts() {
        assert!(parse_url("https://example.com").is_ok());
        assert!(parse_url("file:///etc/passwd").is_err());
        assert!(parse_timeout(Some(0.0), Duration::from_secs(120)).is_err());
        assert!(parse_timeout(Some(121.0), Duration::from_secs(120)).is_err());
        assert_eq!(
            parse_timeout(Some(20.0), Duration::from_secs(10)).unwrap(),
            Duration::from_secs(10)
        );
    }

    #[tokio::test]
    async fn converts_html_to_markdown_and_removes_scripts() {
        let body = "<html><body><h1>Hello</h1><p>Read <a href=\"https://example.com\">more</a>.</p><script>bad()</script></body></html>";
        let (url, _) = server(vec![response("200 OK", "text/html", &[], body)]).await;

        let output = fetch(
            &Client::new(),
            parse_url(&url).unwrap(),
            &CancellationToken::new(),
        )
        .await
        .unwrap();

        assert!(output.contains("# Hello"));
        assert!(output.contains("[more](https://example.com)"));
        assert!(!output.contains("bad()"));
    }

    #[tokio::test]
    async fn rejects_declared_oversized_responses_without_reading_the_body() {
        let raw = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            MAX_RESPONSE_BYTES + 1
        );
        let (url, _) = server(vec![raw]).await;

        let error = fetch(
            &Client::new(),
            parse_url(&url).unwrap(),
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();

        assert!(error.to_string().contains("5MB limit"));
    }

    #[tokio::test]
    async fn cancelled_fetch_returns_cancelled() {
        let body = "<html><body><p>slow</p></body></html>";
        let (url, _) = server(vec![response("200 OK", "text/html", &[], body)]).await;
        let cancellation = CancellationToken::new();
        cancellation.cancel();

        let error = fetch(&Client::new(), parse_url(&url).unwrap(), &cancellation)
            .await
            .unwrap_err();

        assert!(matches!(error, ToolError::Cancelled));
    }

    fn response(status: &str, content_type: &str, headers: &[(&str, &str)], body: &str) -> String {
        let headers = headers
            .iter()
            .map(|(name, value)| format!("{name}: {value}\r\n"))
            .collect::<String>();
        format!(
            "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n{headers}Connection: close\r\n\r\n{body}",
            body.len()
        )
    }

    async fn server(responses: Vec<String>) -> (String, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (tx, rx) = mpsc::channel(responses.len());
        tokio::spawn(async move {
            for response in responses {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut chunk = [0_u8; 1024];
                while !request.ends_with(b"\r\n\r\n") {
                    let read = stream.read(&mut chunk).await.unwrap();
                    if read == 0 {
                        break;
                    }
                    request.extend_from_slice(&chunk[..read]);
                }
                let _ = tx
                    .send(String::from_utf8_lossy(&request).into_owned())
                    .await;
                stream.write_all(response.as_bytes()).await.unwrap();
                stream.shutdown().await.unwrap();
            }
        });
        (format!("http://{address}/page"), rx)
    }
}
