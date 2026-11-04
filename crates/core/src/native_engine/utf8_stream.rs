//! Decode HTTP text incrementally without replacing split UTF-8 code points.
use anyhow::{Context, Result};
use futures::{Stream, StreamExt};

/// Surface gateway error bodies instead of silently treating JSON/HTML as SSE.
pub(super) fn response(
    mut response: reqwest::Response,
) -> impl Stream<Item = Result<String>> + Send {
    async_stream::try_stream! {
        let content_type = response.headers().get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()).unwrap_or_default().to_ascii_lowercase();
        if content_type.contains("application/json") || content_type.contains("text/html") {
            let status = response.status();
            let mut body = Vec::new();
            while body.len() < 8192 {
                let Some(chunk) = response.chunk().await? else { break; };
                let count = chunk.len().min(8192 - body.len());
                body.extend_from_slice(&chunk[..count]);
            }
            let value: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
            let detail = value["error"]["message"].as_str()
                .or_else(|| value["error"].as_str()).or_else(|| value["message"].as_str())
                .map(|s| s.chars().take(1000).collect::<String>()).unwrap_or_default();
            Err(anyhow::anyhow!("Expected an SSE model response, received HTTP {status} ({content_type}). {detail}"))?;
        }
        let stream = decode(response.bytes_stream());
        futures::pin_mut!(stream);
        while let Some(chunk) = stream.next().await {
            yield chunk?;
        }
    }
}

pub(super) fn decode<S, B, E>(stream: S) -> impl Stream<Item = Result<String>> + Send
where
    S: Stream<Item = std::result::Result<B, E>> + Send,
    B: AsRef<[u8]> + Send,
    E: std::error::Error + Send + Sync + 'static,
{
    async_stream::try_stream! {
        futures::pin_mut!(stream);
        let mut pending = Vec::new();
        while let Some(chunk) = stream.next().await {
            pending.extend_from_slice(chunk.context("Stream error")?.as_ref());
            let valid = match std::str::from_utf8(&pending) {
                Ok(_) => pending.len(),
                Err(error) if error.error_len().is_none() => error.valid_up_to(),
                Err(error) => Err(error).context("Invalid UTF-8 in provider stream")?,
            };
            if valid > 0 {
                let text = std::str::from_utf8(&pending[..valid])?.to_owned();
                pending.drain(..valid);
                yield text;
            }
        }
        if !pending.is_empty() {
            Err(anyhow::anyhow!("Provider stream ended inside a UTF-8 character"))?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn preserves_every_byte_boundary_and_reports_incomplete_or_invalid_text() {
        let source = "data: {\"delta\":\"中文 🦀\"}\r\n\r\n \t\n";
        for width in 1..=source.len() {
            let chunks = source
                .as_bytes()
                .chunks(width)
                .map(|chunk| Ok::<_, std::io::Error>(chunk.to_vec()));
            let decoded = decode(futures::stream::iter(chunks));
            futures::pin_mut!(decoded);
            let mut output = String::new();
            while let Some(chunk) = decoded.next().await {
                output.push_str(&chunk.unwrap());
            }
            assert_eq!(output, source, "chunk width {width}");
        }
        for bytes in [vec![0xe4, 0xb8], vec![0xff]] {
            let decoded = decode(futures::stream::iter([Ok::<_, std::io::Error>(bytes)]));
            futures::pin_mut!(decoded);
            assert!(decoded.next().await.unwrap().is_err());
        }
    }

    #[tokio::test]
    async fn custom_clients_preserve_fragmented_sse_text_over_http() {
        use crate::native_engine::{
            anthropic_client::AnthropicClient, openai_client::OpenAIClient,
            provider_manager::ResolvedProvider,
        };
        use axum::{body::Body, routing::post, Router};
        let source = "data: {\"delta\":\"中文 🦀\"}\r\n\r\n \t\n";
        let app = Router::new().route(
            "/{*path}",
            post(move || async move {
                let chunks = async_stream::stream! {
                    for byte in source.as_bytes() {
                        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                        yield Ok::<_, std::io::Error>(vec![*byte]);
                    }
                };
                (
                    [("content-type", "text/event-stream")],
                    Body::from_stream(chunks),
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let provider = ResolvedProvider {
            provider: serde_json::from_value(serde_json::json!({"id":"fixture","name":"fixture","base_url":base,"api_key":"fixture-only","api_format":"openai","enabled":true,"models":[]})).unwrap(),
            model: serde_json::from_value(serde_json::json!({"id":"fixture","name":"fixture","enabled":true,"supports_vision":false,"supports_web_search":false})).unwrap(),
        };
        let streams = [
            OpenAIClient::new()
                .send_message_stream(&provider, vec![], None, vec![], 128)
                .await
                .unwrap(),
            AnthropicClient::new()
                .send_message_stream(&provider, vec![], None, vec![], 128)
                .await
                .unwrap(),
            AnthropicClient::new()
                .send_message_stream_with_thinking(&provider, vec![], None, vec![], 128, false)
                .await
                .unwrap(),
        ];
        for mut stream in streams {
            let mut output = String::new();
            while let Some(chunk) = stream.next().await {
                output.push_str(&chunk.unwrap());
            }
            assert_eq!(output, source);
        }
        server.abort();
    }
}
