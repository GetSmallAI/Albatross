use anyhow::{anyhow, Result};
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::collections::BTreeMap;

use crate::backends::BackendDescriptor;
use crate::cancel::CancellationToken;
use crate::codex_oauth;
use crate::openai::{
    ChatMessage, ChatRequest, StreamChoice, StreamChunk, StreamDelta, ToolCallDelta,
    ToolFunctionDelta, Usage, UserContent, UserContentPart,
};

fn resolve_codex_url(base_url: &str) -> String {
    let normalized = base_url.trim_end_matches('/');
    if normalized.ends_with("/codex/responses") {
        normalized.to_string()
    } else if normalized.ends_with("/codex") {
        format!("{normalized}/responses")
    } else {
        format!("{normalized}/codex/responses")
    }
}

fn user_content_parts(content: &UserContent) -> Vec<Value> {
    match content {
        UserContent::Text(text) => vec![json!({ "type": "input_text", "text": text })],
        UserContent::Parts(parts) => parts
            .iter()
            .map(|part| match part {
                UserContentPart::Text { text } => json!({ "type": "input_text", "text": text }),
                UserContentPart::ImageUrl { image_url } => {
                    json!({ "type": "input_image", "image_url": image_url.url })
                }
            })
            .collect(),
    }
}

fn build_request_body(req: &ChatRequest<'_>) -> Value {
    let model = canonical_codex_model(req.model).unwrap_or(req.model);
    let mut instructions = Vec::new();
    let mut input = Vec::new();

    for message in req.messages {
        match message {
            ChatMessage::System { content } => instructions.push(content.clone()),
            ChatMessage::User { content } => input.push(json!({
                "role": "user",
                "content": user_content_parts(content),
            })),
            ChatMessage::Assistant {
                content,
                tool_calls,
            } => {
                if let Some(text) = content.as_ref().filter(|s| !s.is_empty()) {
                    input.push(json!({
                        "role": "assistant",
                        "content": [{ "type": "output_text", "text": text }],
                    }));
                }
                for call in tool_calls {
                    input.push(json!({
                        "type": "function_call",
                        "call_id": call.id,
                        "name": call.function.name,
                        "arguments": call.function.arguments,
                    }));
                }
            }
            ChatMessage::Tool {
                tool_call_id,
                content,
            } => input.push(json!({
                "type": "function_call_output",
                "call_id": tool_call_id,
                "output": content,
            })),
        }
    }

    let mut body = json!({
        "model": model,
        "store": false,
        "stream": true,
        "instructions": if instructions.is_empty() { "You are a helpful assistant.".to_string() } else { instructions.join("\n\n") },
        "input": input,
        "text": { "verbosity": "low" },
        "include": ["reasoning.encrypted_content"],
        "tool_choice": "auto",
        "parallel_tool_calls": true,
    });

    // Pi's ChatGPT/Codex OAuth provider does not send `max_output_tokens` to
    // `chatgpt.com/backend-api/codex/responses`; the backend rejects it with
    // `Unsupported parameter: max_output_tokens`. Keep Albatross' normal
    // Chat Completions cap out of this adapter.
    if let Some(tools) = req.tools {
        body["tools"] = Value::Array(
            tools
                .iter()
                .map(|tool| {
                    json!({
                        "type": "function",
                        "name": tool.function.name,
                        "description": tool.function.description,
                        "parameters": tool.function.parameters,
                    })
                })
                .collect(),
        );
    }
    body
}

#[derive(Debug, Default)]
struct RawSseParser {
    buf: Vec<u8>,
    data: String,
}

impl RawSseParser {
    fn feed(&mut self, bytes: &[u8]) -> Result<Vec<Value>> {
        self.buf.extend_from_slice(bytes);
        let mut out = Vec::new();
        while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=pos).collect();
            let line = std::str::from_utf8(&line)?.trim_end_matches(['\r', '\n']);
            if line.is_empty() {
                if !self.data.is_empty() {
                    if self.data.trim() != "[DONE]" {
                        out.push(serde_json::from_str(&self.data)?);
                    }
                    self.data.clear();
                }
            } else if let Some(rest) = line.strip_prefix("data:") {
                let rest = rest.strip_prefix(' ').unwrap_or(rest);
                if !self.data.is_empty() {
                    self.data.push('\n');
                }
                self.data.push_str(rest);
            }
        }
        Ok(out)
    }

    fn finalize(&mut self) -> Result<Vec<Value>> {
        let mut out = Vec::new();
        if !self.data.is_empty() && self.data.trim() != "[DONE]" {
            out.push(serde_json::from_str(&self.data)?);
        }
        self.data.clear();
        Ok(out)
    }
}

#[derive(Debug, Default, Clone)]
struct CallState {
    index: usize,
    id: String,
    name: String,
    arguments: String,
}

fn one_content_chunk(delta: String) -> StreamChunk {
    StreamChunk {
        model: None,
        provider: None,
        choices: vec![StreamChoice {
            delta: StreamDelta {
                content: Some(delta),
                reasoning: None,
                tool_calls: None,
            },
        }],
        usage: None,
    }
}

fn one_reasoning_chunk(delta: String) -> StreamChunk {
    StreamChunk {
        model: None,
        provider: None,
        choices: vec![StreamChoice {
            delta: StreamDelta {
                content: None,
                reasoning: Some(delta),
                tool_calls: None,
            },
        }],
        usage: None,
    }
}

fn one_tool_chunk(
    index: usize,
    id: Option<String>,
    name: Option<String>,
    args: Option<String>,
) -> StreamChunk {
    StreamChunk {
        model: None,
        provider: None,
        choices: vec![StreamChoice {
            delta: StreamDelta {
                content: None,
                reasoning: None,
                tool_calls: Some(vec![ToolCallDelta {
                    index: Some(index),
                    id,
                    function: Some(ToolFunctionDelta {
                        name,
                        arguments: args,
                    }),
                }]),
            },
        }],
        usage: None,
    }
}

fn usage_chunk(input: u32, output: u32, cached: u32) -> StreamChunk {
    StreamChunk {
        model: None,
        provider: None,
        choices: Vec::new(),
        usage: Some(Usage {
            prompt_tokens: input,
            completion_tokens: output,
            // The Responses API reports cache reuse under
            // `input_tokens_details.cached_tokens`; surface it like the chat path.
            prompt_tokens_details: (cached > 0).then_some(crate::openai::PromptTokensDetails {
                cached_tokens: cached,
            }),
            cost: None,
        }),
    }
}

fn val_str<'a>(v: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|k| v.get(*k)?.as_str())
}

fn val_usize(v: &Value, keys: &[&str]) -> Option<usize> {
    keys.iter()
        .find_map(|k| v.get(*k)?.as_u64().map(|n| n as usize))
}

fn handle_event<F>(
    event: Value,
    calls: &mut BTreeMap<String, CallState>,
    next_index: &mut usize,
    mut on_chunk: F,
) -> Result<bool>
where
    F: FnMut(StreamChunk),
{
    let kind = event
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    match kind {
        "response.output_text.delta" => {
            if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                if !delta.is_empty() {
                    on_chunk(one_content_chunk(delta.to_string()));
                }
            }
        }
        "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
            if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                if !delta.is_empty() {
                    on_chunk(one_reasoning_chunk(delta.to_string()));
                }
            }
        }
        "response.output_item.added" => {
            let item = event.get("item").unwrap_or(&Value::Null);
            if item.get("type").and_then(Value::as_str) == Some("function_call") {
                let item_id = val_str(item, &["id", "item_id"])
                    .or_else(|| val_str(&event, &["item_id", "id"]))
                    .unwrap_or("")
                    .to_string();
                let call_id = val_str(item, &["call_id"])
                    .unwrap_or(item_id.as_str())
                    .to_string();
                let name = val_str(item, &["name"]).unwrap_or("").to_string();
                let index = val_usize(&event, &["output_index", "index"]).unwrap_or_else(|| {
                    let i = *next_index;
                    *next_index += 1;
                    i
                });
                calls.insert(
                    item_id.clone(),
                    CallState {
                        index,
                        id: call_id.clone(),
                        name: name.clone(),
                        arguments: String::new(),
                    },
                );
                on_chunk(one_tool_chunk(index, Some(call_id), Some(name), None));
            }
        }
        "response.function_call_arguments.delta" => {
            let item_id = val_str(&event, &["item_id", "id"])
                .map(ToString::to_string)
                .unwrap_or_else(|| calls.keys().last().cloned().unwrap_or_default());
            let delta = event
                .get("delta")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if !delta.is_empty() {
                let entry = calls.entry(item_id).or_insert_with(|| {
                    let i = *next_index;
                    *next_index += 1;
                    CallState {
                        index: i,
                        id: String::new(),
                        name: String::new(),
                        arguments: String::new(),
                    }
                });
                entry.arguments.push_str(&delta);
                on_chunk(one_tool_chunk(entry.index, None, None, Some(delta)));
            }
        }
        "response.output_item.done" => {
            let item = event.get("item").unwrap_or(&Value::Null);
            if item.get("type").and_then(Value::as_str) == Some("function_call") {
                let item_id = val_str(item, &["id", "item_id"])
                    .or_else(|| val_str(&event, &["item_id", "id"]))
                    .unwrap_or("")
                    .to_string();
                let call_id = val_str(item, &["call_id"]).unwrap_or("").to_string();
                let name = val_str(item, &["name"]).unwrap_or("").to_string();
                let args = val_str(item, &["arguments"]).unwrap_or("").to_string();
                let entry = calls.entry(item_id).or_insert_with(|| {
                    let i = *next_index;
                    *next_index += 1;
                    CallState {
                        index: i,
                        id: call_id.clone(),
                        name: name.clone(),
                        arguments: String::new(),
                    }
                });
                if entry.id.is_empty() && !call_id.is_empty()
                    || entry.name.is_empty() && !name.is_empty()
                {
                    if !call_id.is_empty() {
                        entry.id = call_id.clone();
                    }
                    if !name.is_empty() {
                        entry.name = name.clone();
                    }
                    on_chunk(one_tool_chunk(
                        entry.index,
                        (!call_id.is_empty()).then_some(call_id),
                        (!name.is_empty()).then_some(name),
                        None,
                    ));
                }
                if !args.is_empty() && args != entry.arguments {
                    let delta = if args.starts_with(&entry.arguments) {
                        args[entry.arguments.len()..].to_string()
                    } else {
                        args.clone()
                    };
                    if !delta.is_empty() {
                        entry.arguments = args;
                        on_chunk(one_tool_chunk(entry.index, None, None, Some(delta)));
                    }
                }
            }
        }
        "response.completed" => {
            if let Some(usage) = event.get("response").and_then(|r| r.get("usage")) {
                let input = usage
                    .get("input_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0) as u32;
                let output = usage
                    .get("output_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0) as u32;
                let cached = usage
                    .get("input_tokens_details")
                    .and_then(|d| d.get("cached_tokens"))
                    .and_then(Value::as_u64)
                    .unwrap_or(0) as u32;
                if input > 0 || output > 0 {
                    on_chunk(usage_chunk(input, output, cached));
                }
            }
            return Ok(true);
        }
        "response.failed" => {
            let error = event
                .get("response")
                .and_then(|r| r.get("error"))
                .and_then(|e| e.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("Codex response failed");
            return Err(anyhow!(error.to_string()));
        }
        "error" => {
            let msg = event
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("Codex stream error");
            return Err(anyhow!(msg.to_string()));
        }
        _ => {}
    }
    Ok(false)
}

pub async fn stream_codex_responses<F>(
    client: &reqwest::Client,
    backend: &BackendDescriptor,
    req: &ChatRequest<'_>,
    cancel: Option<CancellationToken>,
    mut on_chunk: F,
) -> Result<()>
where
    F: FnMut(StreamChunk),
{
    let Some(_model) = canonical_codex_model(req.model) else {
        return Err(anyhow!(
            "{} is not supported with ChatGPT/Codex login. Try one of: {}",
            req.model,
            codex_model_list().join(", ")
        ));
    };
    let (access_token, account_id) = codex_oauth::access_token(client).await?;
    let url = resolve_codex_url(&backend.base_url);
    let body = build_request_body(req);
    let resp = client
        .post(url)
        .bearer_auth(&access_token)
        .header("chatgpt-account-id", account_id)
        // Pre-rename name on purpose — see the note in codex_oauth.rs.
        .header("originator", "small-harness")
        .header("OpenAI-Beta", "responses=experimental")
        .header("accept", "text/event-stream")
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!("Codex HTTP {}: {}", status, body.trim()));
    }

    let mut stream = resp.bytes_stream();
    let mut parser = RawSseParser::default();
    let mut calls = BTreeMap::new();
    let mut next_index = 0usize;
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
        let chunk = chunk?;
        for event in parser.feed(&chunk)? {
            if handle_event(event, &mut calls, &mut next_index, &mut on_chunk)? {
                return Ok(());
            }
        }
    }
    for event in parser.finalize()? {
        if handle_event(event, &mut calls, &mut next_index, &mut on_chunk)? {
            return Ok(());
        }
    }
    Ok(())
}

pub fn codex_model_list() -> Vec<String> {
    vec![
        "gpt-5.5".into(),
        "gpt-5.4".into(),
        "gpt-5.4-mini".into(),
        "gpt-5.3-codex-spark".into(),
    ]
}

/// Canonical ChatGPT/Codex OAuth model ids from Pi's current openai-codex
/// catalog.  Accept a few shorthand aliases because users often type `5.5`,
/// but never send those aliases over the wire.
pub fn canonical_codex_model(model: &str) -> Option<&'static str> {
    match model.trim() {
        "gpt-5.5" | "5.5" => Some("gpt-5.5"),
        "gpt-5.4" | "5.4" => Some("gpt-5.4"),
        "gpt-5.4-mini" | "5.4-mini" => Some("gpt-5.4-mini"),
        "gpt-5.3-codex-spark" | "5.3-codex-spark" | "spark" => Some("gpt-5.3-codex-spark"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::openai::{ToolDef, ToolDefFunction};

    #[test]
    fn resolves_codex_url_variants() {
        assert_eq!(
            resolve_codex_url("https://chatgpt.com/backend-api"),
            "https://chatgpt.com/backend-api/codex/responses"
        );
        assert_eq!(
            resolve_codex_url("https://chatgpt.com/backend-api/codex"),
            "https://chatgpt.com/backend-api/codex/responses"
        );
    }

    #[test]
    fn request_body_uses_responses_tools_shape() {
        let messages = [
            ChatMessage::System {
                content: "sys".into(),
            },
            ChatMessage::User {
                content: "hi".into(),
            },
        ];
        let tools = [ToolDef {
            kind: "function",
            function: ToolDefFunction {
                name: "grep".into(),
                description: "search".into(),
                parameters: json!({ "type": "object" }),
            },
        }];
        let req = ChatRequest {
            model: "5.5",
            messages: &messages,
            tools: Some(&tools),
            stream: true,
            stream_options: None,
            max_tokens: None,
            prompt_cache_key: None,
            effort: None,
        };
        let body = build_request_body(&req);
        assert_eq!(body["instructions"], "sys");
        assert_eq!(body["model"], "gpt-5.5");
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["name"], "grep");
    }

    #[test]
    fn canonicalizes_pi_codex_oauth_models_and_aliases() {
        assert_eq!(canonical_codex_model("gpt-5.5"), Some("gpt-5.5"));
        assert_eq!(canonical_codex_model("5.5"), Some("gpt-5.5"));
        assert_eq!(canonical_codex_model("gpt-5-codex"), None);
    }

    #[test]
    fn maps_text_and_tool_events_to_chat_chunks() {
        let mut chunks = Vec::new();
        let mut calls = BTreeMap::new();
        let mut next_index = 0;
        handle_event(
            json!({"type":"response.output_text.delta","delta":"hi"}),
            &mut calls,
            &mut next_index,
            |c| chunks.push(c),
        )
        .unwrap();
        handle_event(json!({"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"grep"}}), &mut calls, &mut next_index, |c| chunks.push(c)).unwrap();
        handle_event(json!({"type":"response.function_call_arguments.delta","item_id":"fc_1","delta":"{\"pattern\":"}), &mut calls, &mut next_index, |c| chunks.push(c)).unwrap();
        assert_eq!(chunks[0].choices[0].delta.content.as_deref(), Some("hi"));
        let call = chunks[1].choices[0].delta.tool_calls.as_ref().unwrap()[0].clone();
        assert_eq!(call.id.as_deref(), Some("call_1"));
        assert_eq!(call.function.unwrap().name.as_deref(), Some("grep"));
    }

    #[test]
    fn response_completed_surfaces_cached_prompt_tokens() {
        let mut chunks = Vec::new();
        let mut calls = BTreeMap::new();
        let mut next_index = 0;
        let done = handle_event(
            json!({
                "type": "response.completed",
                "response": {"usage": {
                    "input_tokens": 1200,
                    "output_tokens": 87,
                    "input_tokens_details": {"cached_tokens": 900}
                }}
            }),
            &mut calls,
            &mut next_index,
            |c| chunks.push(c),
        )
        .unwrap();
        assert!(done);
        let usage = chunks[0].usage.as_ref().unwrap();
        assert_eq!(usage.prompt_tokens, 1200);
        assert_eq!(usage.completion_tokens, 87);
        assert_eq!(usage.cached_tokens(), 900);
    }

    #[test]
    fn response_completed_without_cache_details_reports_zero_cached() {
        let mut chunks = Vec::new();
        let mut calls = BTreeMap::new();
        let mut next_index = 0;
        handle_event(
            json!({
                "type": "response.completed",
                "response": {"usage": {"input_tokens": 40, "output_tokens": 10}}
            }),
            &mut calls,
            &mut next_index,
            |c| chunks.push(c),
        )
        .unwrap();
        let usage = chunks[0].usage.as_ref().unwrap();
        assert_eq!(usage.cached_tokens(), 0);
        assert!(usage.prompt_tokens_details.is_none());
    }
}
