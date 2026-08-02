use std::collections::BTreeMap;

use anyhow::{anyhow, Result};
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::backends::BackendDescriptor;
use crate::cancel::CancellationToken;
use crate::openai::{
    ChatMessage, ChatRequest, ImageUrl, PromptTokensDetails, SseEvent, StreamChoice, StreamChunk,
    StreamDelta, ToolCallDelta, ToolFunctionDelta, Usage, UserContent, UserContentPart,
};

const ANTHROPIC_VERSION: &str = "2023-06-01";

pub fn model_supports_effort(model: &str) -> bool {
    [
        "claude-fable-5",
        "claude-mythos-5",
        "claude-mythos-preview",
        "claude-opus-5",
        "claude-opus-4-8",
        "claude-opus-4-7",
        "claude-opus-4-6",
        "claude-opus-4-5",
        "claude-sonnet-5",
        "claude-sonnet-4-6",
    ]
    .iter()
    .any(|prefix| model == *prefix || model.starts_with(&format!("{prefix}-")))
}

pub fn effective_effort(
    model: &str,
    requested: crate::model_system::EffortLevel,
) -> Option<&'static str> {
    if !model_supports_effort(model) {
        return None;
    }
    let supports_xhigh = [
        "claude-fable-5",
        "claude-mythos-5",
        "claude-opus-5",
        "claude-opus-4-8",
        "claude-opus-4-7",
        "claude-sonnet-5",
    ]
    .iter()
    .any(|prefix| model == *prefix || model.starts_with(&format!("{prefix}-")));
    let supports_max = !model.starts_with("claude-opus-4-5");
    Some(match requested {
        crate::model_system::EffortLevel::None
        | crate::model_system::EffortLevel::Minimal
        | crate::model_system::EffortLevel::Low => "low",
        crate::model_system::EffortLevel::Medium => "medium",
        crate::model_system::EffortLevel::High => "high",
        crate::model_system::EffortLevel::XHigh if supports_xhigh => "xhigh",
        crate::model_system::EffortLevel::XHigh => "high",
        crate::model_system::EffortLevel::Max if supports_max => "max",
        crate::model_system::EffortLevel::Max => "high",
    })
}

pub async fn list_models(
    client: &reqwest::Client,
    backend: &BackendDescriptor,
) -> Result<Vec<String>> {
    let url = format!("{}/models", backend.base_url.trim_end_matches('/'));
    let response = anthropic_request(client.get(url), backend).send().await?;
    if !response.status().is_success() {
        return Err(response_error(response).await);
    }

    #[derive(Deserialize)]
    struct ModelsResponse {
        data: Vec<Model>,
    }
    #[derive(Deserialize)]
    struct Model {
        id: String,
    }

    let parsed: ModelsResponse = response.json().await?;
    Ok(parsed.data.into_iter().map(|model| model.id).collect())
}

pub async fn chat_oneshot(
    client: &reqwest::Client,
    backend: &BackendDescriptor,
    req: &ChatRequest<'_>,
) -> Result<()> {
    let url = format!("{}/messages", backend.base_url.trim_end_matches('/'));
    let body = build_request_body(req)?;
    let response = anthropic_request(client.post(url), backend)
        .json(&body)
        .send()
        .await?;
    if !response.status().is_success() {
        return Err(response_error(response).await);
    }
    Ok(())
}

pub async fn stream_chat<F>(
    client: &reqwest::Client,
    backend: &BackendDescriptor,
    req: &ChatRequest<'_>,
    cancel: Option<CancellationToken>,
    mut on_chunk: F,
) -> Result<()>
where
    F: FnMut(StreamChunk),
{
    let url = format!("{}/messages", backend.base_url.trim_end_matches('/'));
    let body = build_request_body(req)?;
    let response = anthropic_request(client.post(url), backend)
        .json(&body)
        .send()
        .await?;
    if !response.status().is_success() {
        return Err(response_error(response).await);
    }

    let mut stream = response.bytes_stream();
    let mut parser = AnthropicSseParser::default();
    loop {
        let next = if let Some(cancel) = cancel.clone() {
            tokio::select! {
                _ = cancel.cancelled() => return Err(anyhow!("cancelled")),
                chunk = stream.next() => chunk,
            }
        } else {
            stream.next().await
        };
        let Some(chunk) = next else {
            break;
        };
        for event in parser.feed(&chunk?)? {
            match event {
                SseEvent::Chunk(chunk) => on_chunk(chunk),
                SseEvent::Done => return Ok(()),
            }
        }
    }
    for event in parser.finalize()? {
        if let SseEvent::Chunk(chunk) = event {
            on_chunk(chunk);
        }
    }
    Ok(())
}

fn anthropic_request(
    request: reqwest::RequestBuilder,
    backend: &BackendDescriptor,
) -> reqwest::RequestBuilder {
    request
        .header("x-api-key", &backend.api_key)
        .header("anthropic-version", ANTHROPIC_VERSION)
}

async fn response_error(response: reqwest::Response) -> anyhow::Error {
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    anyhow!("HTTP {status}: {}", body.trim())
}

fn build_request_body(req: &ChatRequest<'_>) -> Result<Value> {
    let mut system = Vec::new();
    let mut messages = Vec::new();
    let mut tool_results = Vec::new();

    for message in req.messages {
        if !matches!(message, ChatMessage::Tool { .. }) {
            flush_tool_results(&mut messages, &mut tool_results);
        }
        match message {
            ChatMessage::System { content } => system.push(content.clone()),
            ChatMessage::User { content } => messages.push(json!({
                "role": "user",
                "content": anthropic_user_content(content)?,
            })),
            ChatMessage::Assistant {
                content,
                tool_calls,
                provider_content,
            } => {
                let blocks = if provider_content.is_empty() {
                    let mut blocks = Vec::new();
                    if let Some(text) = content.as_ref().filter(|text| !text.is_empty()) {
                        blocks.push(json!({ "type": "text", "text": text }));
                    }
                    blocks.extend(tool_calls.iter().map(|call| {
                        let input = serde_json::from_str::<Value>(&call.function.arguments)
                            .unwrap_or_else(|_| json!({}));
                        json!({
                            "type": "tool_use",
                            "id": call.id,
                            "name": call.function.name,
                            "input": input,
                        })
                    }));
                    blocks
                } else {
                    provider_content.clone()
                };
                if !blocks.is_empty() {
                    messages.push(json!({ "role": "assistant", "content": blocks }));
                }
            }
            ChatMessage::Tool {
                tool_call_id,
                content,
            } => tool_results.push(json!({
                "type": "tool_result",
                "tool_use_id": tool_call_id,
                "content": content,
            })),
        }
    }
    flush_tool_results(&mut messages, &mut tool_results);

    let max_tokens = req.max_tokens.unwrap_or(match req.effort {
        Some(crate::model_system::EffortLevel::XHigh | crate::model_system::EffortLevel::Max) => {
            65_536
        }
        _ => 16_384,
    });
    let mut body = json!({
        "model": req.model,
        "messages": messages,
        "max_tokens": max_tokens,
        "stream": req.stream,
    });
    let object = body
        .as_object_mut()
        .ok_or_else(|| anyhow!("Anthropic request did not serialize to an object"))?;
    if !system.is_empty() {
        object.insert("system".into(), Value::String(system.join("\n\n")));
    }
    if let Some(tools) = req.tools {
        object.insert(
            "tools".into(),
            Value::Array(
                tools
                    .iter()
                    .map(|tool| {
                        json!({
                            "name": tool.function.name,
                            "description": tool.function.description,
                            "input_schema": tool.function.parameters,
                        })
                    })
                    .collect(),
            ),
        );
    }
    if let Some(effort) = req
        .effort
        .and_then(|effort| effective_effort(req.model, effort))
    {
        object.insert("output_config".into(), json!({ "effort": effort }));
    }
    Ok(body)
}

fn flush_tool_results(messages: &mut Vec<Value>, tool_results: &mut Vec<Value>) {
    if !tool_results.is_empty() {
        messages.push(json!({
            "role": "user",
            "content": std::mem::take(tool_results),
        }));
    }
}

fn anthropic_user_content(content: &UserContent) -> Result<Value> {
    match content {
        UserContent::Text(text) => Ok(Value::String(text.clone())),
        UserContent::Parts(parts) => Ok(Value::Array(
            parts
                .iter()
                .map(|part| match part {
                    UserContentPart::Text { text } => Ok(json!({
                        "type": "text",
                        "text": text,
                    })),
                    UserContentPart::ImageUrl { image_url } => anthropic_image(image_url),
                })
                .collect::<Result<Vec<_>>>()?,
        )),
    }
}

fn anthropic_image(image: &ImageUrl) -> Result<Value> {
    if let Some(data) = image.url.strip_prefix("data:") {
        let (media_type, encoded) = data
            .split_once(";base64,")
            .ok_or_else(|| anyhow!("Anthropic image data URL must be base64 encoded"))?;
        return Ok(json!({
            "type": "image",
            "source": {
                "type": "base64",
                "media_type": media_type,
                "data": encoded,
            }
        }));
    }
    Ok(json!({
        "type": "image",
        "source": { "type": "url", "url": image.url },
    }))
}

#[derive(Default)]
struct AnthropicSseParser {
    buf: Vec<u8>,
    data: String,
    blocks: BTreeMap<usize, Value>,
    tool_json: BTreeMap<usize, String>,
}

impl AnthropicSseParser {
    fn feed(&mut self, bytes: &[u8]) -> Result<Vec<SseEvent>> {
        self.buf.extend_from_slice(bytes);
        let mut events = Vec::new();
        while let Some(position) = self.buf.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=position).collect();
            let line = std::str::from_utf8(&line)
                .map_err(|error| anyhow!("non-utf8 Anthropic SSE line: {error}"))?
                .trim_end_matches(['\r', '\n']);
            if line.is_empty() {
                if !self.data.is_empty() {
                    let data = std::mem::take(&mut self.data);
                    events.extend(self.process_data(&data)?);
                }
            } else if let Some(data) = line.strip_prefix("data:") {
                if !self.data.is_empty() {
                    self.data.push('\n');
                }
                self.data.push_str(data.strip_prefix(' ').unwrap_or(data));
            }
        }
        Ok(events)
    }

    fn finalize(&mut self) -> Result<Vec<SseEvent>> {
        if self.data.is_empty() {
            return Ok(Vec::new());
        }
        let data = std::mem::take(&mut self.data);
        self.process_data(&data)
    }

    fn process_data(&mut self, data: &str) -> Result<Vec<SseEvent>> {
        let value: Value = serde_json::from_str(data)
            .map_err(|error| anyhow!("invalid Anthropic SSE event: {error}"))?;
        let event_type = value.get("type").and_then(Value::as_str).unwrap_or("");
        let mut events = Vec::new();
        match event_type {
            "message_start" => {
                let message = &value["message"];
                events.push(SseEvent::Chunk(StreamChunk {
                    model: message
                        .get("model")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    provider: Some("anthropic".into()),
                    choices: Vec::new(),
                    usage: usage_from(&message["usage"], false),
                    provider_content_block: None,
                }));
            }
            "content_block_start" => {
                let index = value["index"].as_u64().unwrap_or(0) as usize;
                let block = value["content_block"].clone();
                self.blocks.insert(index, block.clone());
                if block["type"] == "tool_use" {
                    events.push(SseEvent::Chunk(delta_chunk(StreamDelta {
                        tool_calls: Some(vec![ToolCallDelta {
                            index: Some(index),
                            id: block.get("id").and_then(Value::as_str).map(str::to_string),
                            function: Some(ToolFunctionDelta {
                                name: block
                                    .get("name")
                                    .and_then(Value::as_str)
                                    .map(str::to_string),
                                arguments: None,
                            }),
                        }]),
                        ..StreamDelta::default()
                    })));
                }
            }
            "content_block_delta" => {
                let index = value["index"].as_u64().unwrap_or(0) as usize;
                let delta = &value["delta"];
                match delta.get("type").and_then(Value::as_str).unwrap_or("") {
                    "text_delta" => {
                        let text = delta.get("text").and_then(Value::as_str).unwrap_or("");
                        append_block_string(self.blocks.get_mut(&index), "text", text);
                        events.push(SseEvent::Chunk(delta_chunk(StreamDelta {
                            content: Some(text.to_string()),
                            ..StreamDelta::default()
                        })));
                    }
                    "thinking_delta" => {
                        let thinking = delta.get("thinking").and_then(Value::as_str).unwrap_or("");
                        append_block_string(self.blocks.get_mut(&index), "thinking", thinking);
                        events.push(SseEvent::Chunk(delta_chunk(StreamDelta {
                            reasoning: Some(thinking.to_string()),
                            ..StreamDelta::default()
                        })));
                    }
                    "signature_delta" => {
                        let signature =
                            delta.get("signature").and_then(Value::as_str).unwrap_or("");
                        append_block_string(self.blocks.get_mut(&index), "signature", signature);
                    }
                    "input_json_delta" => {
                        let partial = delta
                            .get("partial_json")
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        self.tool_json.entry(index).or_default().push_str(partial);
                        events.push(SseEvent::Chunk(delta_chunk(StreamDelta {
                            tool_calls: Some(vec![ToolCallDelta {
                                index: Some(index),
                                id: None,
                                function: Some(ToolFunctionDelta {
                                    name: None,
                                    arguments: Some(partial.to_string()),
                                }),
                            }]),
                            ..StreamDelta::default()
                        })));
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                let index = value["index"].as_u64().unwrap_or(0) as usize;
                if let Some(input) = self.tool_json.remove(&index) {
                    let parsed = serde_json::from_str(&input).unwrap_or_else(|_| json!({}));
                    if let Some(block) = self.blocks.get_mut(&index).and_then(Value::as_object_mut)
                    {
                        block.insert("input".into(), parsed);
                    }
                }
                if let Some(block) = self.blocks.remove(&index) {
                    events.push(SseEvent::Chunk(StreamChunk {
                        model: None,
                        provider: None,
                        choices: Vec::new(),
                        usage: None,
                        provider_content_block: Some(block),
                    }));
                }
            }
            "message_delta" => events.push(SseEvent::Chunk(StreamChunk {
                model: None,
                provider: None,
                choices: Vec::new(),
                usage: usage_from(&value["usage"], true),
                provider_content_block: None,
            })),
            "message_stop" => events.push(SseEvent::Done),
            "error" => {
                let message = value["error"]["message"]
                    .as_str()
                    .unwrap_or("unknown Anthropic streaming error");
                return Err(anyhow!("Anthropic stream error: {message}"));
            }
            "ping" => {}
            _ => {}
        }
        Ok(events)
    }
}

fn append_block_string(block: Option<&mut Value>, field: &str, value: &str) {
    let Some(object) = block.and_then(Value::as_object_mut) else {
        return;
    };
    let target = object
        .entry(field.to_string())
        .or_insert_with(|| Value::String(String::new()));
    if let Some(target) = target.as_str().map(str::to_string) {
        *object.get_mut(field).expect("entry exists") = Value::String(format!("{target}{value}"));
    }
}

fn usage_from(value: &Value, output_only: bool) -> Option<Usage> {
    if !value.is_object() {
        return None;
    }
    let input = if output_only {
        0
    } else {
        value["input_tokens"].as_u64().unwrap_or(0) as u32
    };
    let cached = if output_only {
        0
    } else {
        value["cache_read_input_tokens"].as_u64().unwrap_or(0) as u32
    };
    let cache_creation = if output_only {
        0
    } else {
        value["cache_creation_input_tokens"].as_u64().unwrap_or(0) as u32
    };
    let output = value["output_tokens"].as_u64().unwrap_or(0) as u32;
    Some(Usage {
        prompt_tokens: input.saturating_add(cached).saturating_add(cache_creation),
        completion_tokens: output,
        cost: None,
        prompt_tokens_details: (cached > 0).then_some(PromptTokensDetails {
            cached_tokens: cached,
        }),
        cache_creation_input_tokens: cache_creation,
    })
}

fn delta_chunk(delta: StreamDelta) -> StreamChunk {
    StreamChunk {
        model: None,
        provider: None,
        choices: vec![StreamChoice { delta }],
        usage: None,
        provider_content_block: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backends::{BackendName, OpenRouterConfig};
    use crate::openai::{ChatMessage, StreamOptions, ToolDef, ToolDefFunction};
    use std::io::{Read, Write};
    use std::net::TcpListener;

    fn backend() -> BackendDescriptor {
        BackendDescriptor {
            name: BackendName::Anthropic,
            base_url: "https://api.anthropic.com/v1".into(),
            api_key: "sk-ant-test".into(),
            is_local: false,
            openrouter: OpenRouterConfig::default(),
        }
    }

    #[test]
    fn request_uses_messages_shape_and_anthropic_effort() {
        let messages = vec![
            ChatMessage::System {
                content: "system prompt".into(),
            },
            ChatMessage::User {
                content: "hello".into(),
            },
        ];
        let tools = vec![ToolDef {
            kind: "function",
            function: ToolDefFunction {
                name: "shell".into(),
                description: "Run a command".into(),
                parameters: json!({"type":"object"}),
            },
        }];
        let req = ChatRequest {
            model: "claude-sonnet-5",
            messages: &messages,
            tools: Some(&tools),
            stream: true,
            stream_options: Some(StreamOptions {
                include_usage: true,
            }),
            max_tokens: None,
            prompt_cache_key: None,
            effort: Some(crate::model_system::EffortLevel::Medium),
        };
        let body = build_request_body(&req).unwrap();
        assert_eq!(body["system"], "system prompt");
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["tools"][0]["input_schema"]["type"], "object");
        assert_eq!(body["output_config"]["effort"], "medium");
        assert_eq!(body["max_tokens"], 16_384);
        let request = anthropic_request(
            reqwest::Client::new().post("https://api.anthropic.com/v1/messages"),
            &backend(),
        )
        .build()
        .unwrap();
        assert_eq!(request.headers()["x-api-key"], "sk-ant-test");
        assert_eq!(request.headers()["anthropic-version"], ANTHROPIC_VERSION);
        assert!(request.headers().get("authorization").is_none());
    }

    #[test]
    fn effort_is_model_aware_and_never_sent_to_unsupported_models() {
        assert_eq!(
            effective_effort("claude-sonnet-5", crate::model_system::EffortLevel::XHigh),
            Some("xhigh")
        );
        assert_eq!(
            effective_effort("claude-sonnet-4-6", crate::model_system::EffortLevel::XHigh),
            Some("high")
        );
        assert_eq!(
            effective_effort("claude-haiku-4-5", crate::model_system::EffortLevel::Low),
            None
        );
    }

    #[test]
    fn parser_maps_text_tools_usage_and_native_blocks() {
        let input = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"model\":\"claude-sonnet-5\",\"usage\":{\"input_tokens\":10,\"cache_read_input_tokens\":4,\"cache_creation_input_tokens\":2}}}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hello\"}}\n\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"shell\",\"input\":{}}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"command\\\":\\\"pwd\\\"}\"}}\n\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":7}}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n"
        );
        let events = AnthropicSseParser::default()
            .feed(input.as_bytes())
            .unwrap();
        let chunks: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                SseEvent::Chunk(chunk) => Some(chunk),
                SseEvent::Done => None,
            })
            .collect();
        let usage = chunks[0].usage.as_ref().unwrap();
        assert_eq!(usage.prompt_tokens, 16);
        assert_eq!(usage.cached_tokens(), 4);
        assert_eq!(usage.cache_creation_tokens(), 2);
        assert!(chunks.iter().any(|chunk| {
            chunk
                .choices
                .first()
                .and_then(|choice| choice.delta.content.as_deref())
                == Some("hello")
        }));
        let blocks: Vec<_> = chunks
            .iter()
            .filter_map(|chunk| chunk.provider_content_block.as_ref())
            .collect();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[1]["input"]["command"], "pwd");
        assert!(matches!(events.last(), Some(SseEvent::Done)));
    }

    #[test]
    fn request_round_trips_native_thinking_and_groups_tool_results() {
        let messages = vec![
            ChatMessage::Assistant {
                content: None,
                tool_calls: Vec::new(),
                provider_content: vec![
                    json!({"type":"thinking","thinking":"","signature":"opaque"}),
                    json!({"type":"tool_use","id":"toolu_1","name":"shell","input":{"command":"pwd"}}),
                ],
            },
            ChatMessage::Tool {
                tool_call_id: "toolu_1".into(),
                content: "/tmp".into(),
            },
        ];
        let req = ChatRequest {
            model: "claude-sonnet-5",
            messages: &messages,
            tools: None,
            stream: true,
            stream_options: None,
            max_tokens: None,
            prompt_cache_key: None,
            effort: None,
        };
        let body = build_request_body(&req).unwrap();
        assert_eq!(body["messages"][0]["content"][0]["signature"], "opaque");
        assert_eq!(body["messages"][1]["role"], "user");
        assert_eq!(body["messages"][1]["content"][0]["tool_use_id"], "toolu_1");
    }

    #[tokio::test]
    async fn native_stream_uses_anthropic_endpoint_headers_and_body() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 8192];
            let bytes = stream.read(&mut request).unwrap();
            let request = String::from_utf8_lossy(&request[..bytes]);
            assert!(request.starts_with("POST /v1/messages HTTP/1.1"));
            assert!(request
                .to_ascii_lowercase()
                .contains("x-api-key: sk-ant-test"));
            assert!(request
                .to_ascii_lowercase()
                .contains("anthropic-version: 2023-06-01"));
            assert!(!request.to_ascii_lowercase().contains("authorization:"));
            assert!(request.contains("\"model\":\"claude-sonnet-5\""));
            assert!(request.contains("\"effort\":\"low\""));

            let body = concat!(
                "event: message_start\n",
                "data: {\"type\":\"message_start\",\"message\":{\"model\":\"claude-sonnet-5\",\"usage\":{\"input_tokens\":3}}}\n\n",
                "event: content_block_start\n",
                "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
                "event: content_block_delta\n",
                "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hello\"}}\n\n",
                "event: content_block_stop\n",
                "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
                "event: message_delta\n",
                "data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":1}}\n\n",
                "event: message_stop\n",
                "data: {\"type\":\"message_stop\"}\n\n"
            );
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
        });

        let backend = BackendDescriptor {
            name: BackendName::Anthropic,
            base_url: format!("http://{address}/v1"),
            api_key: "sk-ant-test".into(),
            is_local: false,
            openrouter: OpenRouterConfig::default(),
        };
        let messages = vec![ChatMessage::User {
            content: "hi".into(),
        }];
        let request = ChatRequest {
            model: "claude-sonnet-5",
            messages: &messages,
            tools: None,
            stream: true,
            stream_options: None,
            max_tokens: Some(100),
            prompt_cache_key: None,
            effort: Some(crate::model_system::EffortLevel::Minimal),
        };
        let mut text = String::new();
        stream_chat(&reqwest::Client::new(), &backend, &request, None, |chunk| {
            if let Some(delta) = chunk
                .choices
                .first()
                .and_then(|choice| choice.delta.content.as_deref())
            {
                text.push_str(delta);
            }
        })
        .await
        .unwrap();
        server.join().unwrap();
        assert_eq!(text, "hello");
    }
}
