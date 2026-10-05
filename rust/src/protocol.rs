use serde_json::{Value, json};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Chat,
    Responses,
    Claude,
    Gemini,
}

#[derive(Clone, Debug)]
enum Part {
    Text(String),
    Call {
        id: String,
        name: String,
        arguments: Value,
    },
    Result {
        id: String,
        content: String,
    },
}

#[derive(Clone, Debug)]
struct Message {
    role: String,
    parts: Vec<Part>,
}

fn required_str<'a>(value: &'a Value, key: &str) -> Result<&'a str, String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing or invalid {key}"))
}

fn only_keys(value: &Value, allowed: &[&str], context: &str) -> Result<(), String> {
    let object = value
        .as_object()
        .ok_or_else(|| format!("{context} must be an object"))?;
    for key in object.keys() {
        if !allowed.contains(&key.as_str()) {
            return Err(format!("unsupported {context} field: {key}"));
        }
    }
    Ok(())
}

fn text_parts(value: &Value) -> Result<Vec<Part>, String> {
    match value {
        Value::String(s) => Ok(vec![Part::Text(s.clone())]),
        Value::Array(items) => items
            .iter()
            .map(|item| match item.get("type").and_then(Value::as_str) {
                Some("text" | "input_text" | "output_text") => {
                    only_keys(item, &["type", "text", "annotations"], "text block")?;
                    if item.get("annotations").is_some_and(|v| v != &json!([])) {
                        return Err("annotated text blocks are not supported".into());
                    }
                    Ok(Part::Text(required_str(item, "text")?.to_owned()))
                }
                Some(kind) => Err(format!("unsupported content block: {kind}")),
                None => Err("content block missing type".into()),
            })
            .collect(),
        _ => Err("content must be text or an array of supported blocks".into()),
    }
}

fn chat_messages(body: &Value) -> Result<Vec<Message>, String> {
    let input = body
        .get("messages")
        .and_then(Value::as_array)
        .ok_or("messages must be an array")?;
    let mut out = Vec::new();
    for item in input {
        only_keys(
            item,
            &["role", "content", "tool_call_id", "tool_calls", "name"],
            "chat message",
        )?;
        let role = required_str(item, "role")?.to_owned();
        let mut parts = if role == "tool" {
            vec![Part::Result {
                id: required_str(item, "tool_call_id")?.to_owned(),
                content: required_str(item, "content")?.to_owned(),
            }]
        } else {
            match item.get("content") {
                Some(Value::Null) | None if item.get("tool_calls").is_some() => vec![],
                Some(v) => text_parts(v)?,
                None => return Err("message missing content".into()),
            }
        };
        if let Some(calls) = item.get("tool_calls") {
            for call in calls.as_array().ok_or("tool_calls must be an array")? {
                only_keys(call, &["id", "type", "function"], "chat tool call")?;
                if call.get("type").and_then(Value::as_str) != Some("function") {
                    return Err("unsupported tool call type".into());
                }
                let function = call.get("function").ok_or("tool call missing function")?;
                only_keys(function, &["name", "arguments"], "chat function")?;
                let arguments = serde_json::from_str(required_str(function, "arguments")?)
                    .map_err(|_| "tool call arguments must be valid JSON")?;
                parts.push(Part::Call {
                    id: required_str(call, "id")?.to_owned(),
                    name: required_str(function, "name")?.to_owned(),
                    arguments,
                });
            }
        }
        if !matches!(
            role.as_str(),
            "system" | "developer" | "user" | "assistant" | "tool"
        ) {
            return Err(format!("unsupported role: {role}"));
        }
        out.push(Message { role, parts });
    }
    Ok(out)
}

fn responses_messages(body: &Value) -> Result<Vec<Message>, String> {
    let mut out = Vec::new();
    if let Some(instructions) = body.get("instructions") {
        out.push(Message {
            role: "system".into(),
            parts: text_parts(instructions)?,
        });
    }
    let input = body.get("input").ok_or("missing input")?;
    if input.is_string() {
        out.push(Message {
            role: "user".into(),
            parts: text_parts(input)?,
        });
        return Ok(out);
    }
    for item in input.as_array().ok_or("input must be text or an array")? {
        match item.get("type").and_then(Value::as_str) {
            Some("function_call") => {
                only_keys(
                    item,
                    &["type", "call_id", "name", "arguments", "id", "status"],
                    "Responses function call",
                )?;
                out.push(Message {
                    role: "assistant".into(),
                    parts: vec![Part::Call {
                        id: required_str(item, "call_id")?.to_owned(),
                        name: required_str(item, "name")?.to_owned(),
                        arguments: serde_json::from_str(required_str(item, "arguments")?)
                            .map_err(|_| "function_call arguments must be JSON")?,
                    }],
                })
            }
            Some("function_call_output") => {
                only_keys(
                    item,
                    &["type", "call_id", "output"],
                    "Responses function result",
                )?;
                out.push(Message {
                    role: "tool".into(),
                    parts: vec![Part::Result {
                        id: required_str(item, "call_id")?.to_owned(),
                        content: required_str(item, "output")?.to_owned(),
                    }],
                })
            }
            None | Some("message") => {
                only_keys(
                    item,
                    &["type", "role", "content", "id", "status"],
                    "Responses message",
                )?;
                out.push(Message {
                    role: required_str(item, "role")?.to_owned(),
                    parts: text_parts(item.get("content").ok_or("message missing content")?)?,
                })
            }
            Some(kind) => return Err(format!("unsupported Responses input item: {kind}")),
        }
    }
    Ok(out)
}

fn claude_messages(body: &Value) -> Result<Vec<Message>, String> {
    let mut out = Vec::new();
    if let Some(system) = body.get("system") {
        out.push(Message {
            role: "system".into(),
            parts: text_parts(system)?,
        });
    }
    for item in body
        .get("messages")
        .and_then(Value::as_array)
        .ok_or("messages must be an array")?
    {
        only_keys(item, &["role", "content"], "Claude message")?;
        let role = required_str(item, "role")?.to_owned();
        let content = item.get("content").ok_or("message missing content")?;
        let parts = if let Some(blocks) = content.as_array() {
            blocks
                .iter()
                .map(|block| match block.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        only_keys(block, &["type", "text"], "Claude text block")?;
                        Ok(Part::Text(required_str(block, "text")?.to_owned()))
                    }
                    Some("tool_use") => {
                        only_keys(
                            block,
                            &["type", "id", "name", "input"],
                            "Claude tool_use block",
                        )?;
                        Ok(Part::Call {
                            id: required_str(block, "id")?.to_owned(),
                            name: required_str(block, "name")?.to_owned(),
                            arguments: block
                                .get("input")
                                .cloned()
                                .ok_or("tool_use missing input")?,
                        })
                    }
                    Some("tool_result") => {
                        only_keys(
                            block,
                            &["type", "tool_use_id", "content"],
                            "Claude tool_result block",
                        )?;
                        Ok(Part::Result {
                            id: required_str(block, "tool_use_id")?.to_owned(),
                            content: required_str(block, "content")?.to_owned(),
                        })
                    }
                    Some(kind) => Err(format!("unsupported Claude content block: {kind}")),
                    None => Err("Claude content block missing type".into()),
                })
                .collect::<Result<Vec<_>, _>>()?
        } else {
            text_parts(content)?
        };
        if !matches!(role.as_str(), "user" | "assistant") {
            return Err(format!("unsupported Claude role: {role}"));
        }
        out.push(Message { role, parts });
    }
    Ok(out)
}

fn gemini_messages(body: &Value) -> Result<Vec<Message>, String> {
    let mut out = Vec::new();
    if let Some(system) = body.get("systemInstruction") {
        only_keys(system, &["parts", "role"], "Gemini system instruction")?;
        let parts = system
            .get("parts")
            .and_then(Value::as_array)
            .ok_or("systemInstruction.parts must be an array")?;
        out.push(Message {
            role: "system".into(),
            parts: parts
                .iter()
                .map(|part| {
                    only_keys(part, &["text"], "Gemini system part")?;
                    Ok(Part::Text(required_str(part, "text")?.to_owned()))
                })
                .collect::<Result<_, String>>()?,
        });
    }
    for item in body
        .get("contents")
        .and_then(Value::as_array)
        .ok_or("contents must be an array")?
    {
        only_keys(item, &["role", "parts"], "Gemini content")?;
        let role = match item.get("role").and_then(Value::as_str).unwrap_or("user") {
            "user" => "user",
            "model" => "assistant",
            kind => return Err(format!("unsupported Gemini role: {kind}")),
        }
        .to_owned();
        let parts = item
            .get("parts")
            .and_then(Value::as_array)
            .ok_or("Gemini parts must be an array")?;
        let parts = parts
            .iter()
            .map(|part| {
                if part.get("text").is_some() {
                    only_keys(part, &["text"], "Gemini text part")?;
                } else if part.get("functionCall").is_some() {
                    only_keys(part, &["functionCall"], "Gemini function call part")?;
                } else if part.get("functionResponse").is_some() {
                    only_keys(part, &["functionResponse"], "Gemini function response part")?;
                }
                if let Some(s) = part.get("text").and_then(Value::as_str) {
                    return Ok(Part::Text(s.to_owned()));
                }
                if let Some(call) = part.get("functionCall") {
                    only_keys(call, &["name", "args"], "Gemini function call")?;
                    return Ok(Part::Call {
                        id: required_str(call, "name")?.to_owned(),
                        name: required_str(call, "name")?.to_owned(),
                        arguments: call.get("args").cloned().unwrap_or_else(|| json!({})),
                    });
                }
                if let Some(result) = part.get("functionResponse") {
                    only_keys(result, &["name", "response"], "Gemini function response")?;
                    return Ok(Part::Result {
                        id: required_str(result, "name")?.to_owned(),
                        content: result
                            .get("response")
                            .map(Value::to_string)
                            .unwrap_or_default(),
                    });
                }
                Err("unsupported Gemini content part".into())
            })
            .collect::<Result<Vec<_>, String>>()?;
        out.push(Message { role, parts });
    }
    Ok(out)
}

fn check_fields(body: &Value, allowed: &[&str]) -> Result<(), String> {
    only_keys(body, allowed, "cross-format request")
}

pub fn convert_request(source: Format, target: Format, body: &Value) -> Result<Value, String> {
    if source == target {
        return Ok(body.clone());
    }
    let allowed: &[&str] = match source {
        Format::Chat => &[
            "model",
            "messages",
            "stream",
            "temperature",
            "top_p",
            "max_tokens",
            "tools",
            "tool_choice",
        ],
        Format::Responses => &[
            "model",
            "input",
            "instructions",
            "stream",
            "temperature",
            "top_p",
            "max_output_tokens",
            "tools",
            "tool_choice",
        ],
        Format::Claude => &[
            "model",
            "messages",
            "system",
            "stream",
            "temperature",
            "top_p",
            "max_tokens",
            "tools",
            "tool_choice",
        ],
        Format::Gemini => &["contents", "systemInstruction", "generationConfig", "tools"],
    };
    check_fields(body, allowed)?;
    if body.get("tools").is_some() || body.get("tool_choice").is_some() {
        return Err("cross-format tool definitions are not supported".into());
    }
    if source == Format::Gemini
        && let Some(config) = body.get("generationConfig")
    {
        only_keys(
            config,
            &["temperature", "topP", "maxOutputTokens"],
            "Gemini generationConfig",
        )?;
    }
    let messages = match source {
        Format::Chat => chat_messages(body)?,
        Format::Responses => responses_messages(body)?,
        Format::Claude => claude_messages(body)?,
        Format::Gemini => gemini_messages(body)?,
    };
    let mut output = match target {
        Format::Chat => {
            let items = messages.iter().map(|m| {
                if m.role == "tool" {
                    let [Part::Result { id, content }] = m.parts.as_slice() else { return Err("tool message must contain one result".into()); };
                    return Ok(json!({"role":"tool", "tool_call_id":id, "content":content}));
                }
                let mut text = String::new();
                let mut calls = Vec::new();
                for part in &m.parts {
                    match part {
                        Part::Text(s) => text.push_str(s),
                        Part::Call { id, name, arguments } => calls.push(json!({"id":id,"type":"function","function":{"name":name,"arguments":arguments.to_string()}})),
                        Part::Result { .. } => return Err("tool result in non-tool message".into()),
                    }
                }
                let mut item = json!({"role":m.role,"content":text});
                if !calls.is_empty() { item["tool_calls"] = json!(calls); }
                Ok(item)
            }).collect::<Result<Vec<Value>, String>>()?;
            json!({"messages":items})
        }
        Format::Responses => {
            let mut items = Vec::new();
            let mut instructions = Vec::new();
            for m in &messages {
                if matches!(m.role.as_str(), "system" | "developer") {
                    for part in &m.parts {
                        match part {
                            Part::Text(s) => instructions.push(s.as_str()),
                            _ => return Err("system tools unsupported".into()),
                        }
                    }
                    continue;
                }
                let mut content = Vec::new();
                for part in &m.parts {
                    match part {
                        Part::Text(s) => content.push(json!({"type":if m.role == "assistant" { "output_text" } else { "input_text" }, "text":s})),
                        Part::Call { id, name, arguments } => items.push(json!({"type":"function_call","call_id":id,"name":name,"arguments":arguments.to_string()})),
                        Part::Result { id, content } => items.push(json!({"type":"function_call_output","call_id":id,"output":content})),
                    }
                }
                if !content.is_empty() {
                    items.push(json!({"role":m.role,"content":content}));
                }
            }
            let mut output = json!({"input":items});
            if !instructions.is_empty() {
                output["instructions"] = json!(instructions.join("\n"));
            }
            output
        }
        Format::Claude => {
            let mut system = Vec::new();
            let mut items = Vec::new();
            for m in &messages {
                if matches!(m.role.as_str(), "system" | "developer") {
                    for part in &m.parts {
                        match part {
                            Part::Text(s) => system.push(s.as_str()),
                            _ => return Err("system tools unsupported".into()),
                        }
                    }
                    continue;
                }
                let mut content = Vec::new();
                for part in &m.parts {
                    content.push(match part {
                        Part::Text(s) => json!({"type":"text","text":s}),
                        Part::Call {
                            id,
                            name,
                            arguments,
                        } => json!({"type":"tool_use","id":id,"name":name,"input":arguments}),
                        Part::Result { id, content } => {
                            json!({"type":"tool_result","tool_use_id":id,"content":content})
                        }
                    });
                }
                items.push(json!({"role":if m.role == "tool" { "user" } else { &m.role },"content":content}));
            }
            let mut output = json!({"messages":items});
            if !system.is_empty() {
                output["system"] = json!(system.join("\n"));
            }
            output
        }
        Format::Gemini => {
            let mut system = Vec::new();
            let mut items = Vec::new();
            for m in &messages {
                if matches!(m.role.as_str(), "system" | "developer") {
                    for part in &m.parts {
                        match part {
                            Part::Text(s) => system.push(json!({"text":s})),
                            _ => return Err("system tools unsupported".into()),
                        }
                    }
                    continue;
                }
                let parts = m
                    .parts
                    .iter()
                    .map(|part| {
                        Ok(match part {
                            Part::Text(s) => json!({"text":s}),
                            Part::Call {
                                name, arguments, ..
                            } => json!({"functionCall":{"name":name,"args":arguments}}),
                            Part::Result { .. } => {
                                return Err("Gemini tool results need a function name".into());
                            }
                        })
                    })
                    .collect::<Result<Vec<_>, String>>()?;
                items.push(json!({"role":if m.role == "assistant" { "model" } else { "user" },"parts":parts}));
            }
            let mut output = json!({"contents":items});
            if !system.is_empty() {
                output["systemInstruction"] = json!({"parts":system});
            }
            output
        }
    };
    for (from, to) in match (source, target) {
        (Format::Chat, Format::Responses | Format::Claude) => vec![
            (
                "max_tokens",
                if target == Format::Responses {
                    "max_output_tokens"
                } else {
                    "max_tokens"
                },
            ),
            ("temperature", "temperature"),
            ("top_p", "top_p"),
        ],
        (Format::Responses, Format::Chat | Format::Claude) => vec![
            ("max_output_tokens", "max_tokens"),
            ("temperature", "temperature"),
            ("top_p", "top_p"),
        ],
        (Format::Claude, Format::Chat | Format::Responses) => vec![
            (
                "max_tokens",
                if target == Format::Responses {
                    "max_output_tokens"
                } else {
                    "max_tokens"
                },
            ),
            ("temperature", "temperature"),
            ("top_p", "top_p"),
        ],
        _ => vec![],
    } {
        if let Some(value) = body.get(from) {
            output[to] = value.clone();
        }
    }
    if target == Format::Gemini {
        for (from, to) in [
            ("temperature", "temperature"),
            ("top_p", "topP"),
            ("max_tokens", "maxOutputTokens"),
            ("max_output_tokens", "maxOutputTokens"),
        ] {
            if let Some(value) = body.get(from) {
                output["generationConfig"][to] = value.clone();
            }
        }
    } else if source == Format::Gemini
        && let Some(config) = body.get("generationConfig")
    {
        for (from, to) in [
            ("temperature", "temperature"),
            ("topP", "top_p"),
            (
                "maxOutputTokens",
                if target == Format::Responses {
                    "max_output_tokens"
                } else {
                    "max_tokens"
                },
            ),
        ] {
            if let Some(value) = config.get(from) {
                output[to] = value.clone();
            }
        }
    }
    if target == Format::Claude && output.get("max_tokens").is_none() {
        return Err("Claude conversion requires max_tokens".into());
    }
    Ok(output)
}

pub fn convert_response(
    source: Format,
    target: Format,
    body: &Value,
    model: &str,
) -> Result<Value, String> {
    if source == target {
        return Ok(body.clone());
    }
    let truncated = match source {
        Format::Chat => match body
            .pointer("/choices/0/finish_reason")
            .and_then(Value::as_str)
        {
            Some("length") => true,
            Some("stop" | "tool_calls") => false,
            _ => return Err("unsupported chat finish reason".into()),
        },
        Format::Responses => match body.get("status").and_then(Value::as_str) {
            Some("incomplete") => true,
            Some("completed") => false,
            _ => return Err("unsupported Responses status".into()),
        },
        Format::Claude => match body.get("stop_reason").and_then(Value::as_str) {
            Some("max_tokens") => true,
            Some("end_turn" | "stop_sequence" | "tool_use") => false,
            _ => return Err("unsupported Claude stop reason".into()),
        },
        Format::Gemini => match body
            .pointer("/candidates/0/finishReason")
            .and_then(Value::as_str)
        {
            Some("MAX_TOKENS") => true,
            Some("STOP") => false,
            _ => return Err("unsupported Gemini finish reason".into()),
        },
    };
    let (parts, usage) = match source {
        Format::Chat => {
            let message = body
                .pointer("/choices/0/message")
                .ok_or("chat response missing message")?;
            let mut parts = message
                .get("content")
                .and_then(Value::as_str)
                .map(|s| vec![Part::Text(s.to_owned())])
                .unwrap_or_default();
            if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
                for call in calls {
                    let f = call.get("function").ok_or("tool call missing function")?;
                    parts.push(Part::Call {
                        id: required_str(call, "id")?.to_owned(),
                        name: required_str(f, "name")?.to_owned(),
                        arguments: serde_json::from_str(required_str(f, "arguments")?)
                            .map_err(|_| "invalid tool arguments")?,
                    });
                }
            }
            (parts, body.get("usage"))
        }
        Format::Responses => {
            let mut parts = Vec::new();
            for item in body
                .get("output")
                .and_then(Value::as_array)
                .ok_or("Responses output missing")?
            {
                match item.get("type").and_then(Value::as_str) {
                    Some("message") => parts.extend(text_parts(
                        item.get("content").ok_or("message missing content")?,
                    )?),
                    Some("function_call") => parts.push(Part::Call {
                        id: required_str(item, "call_id")?.to_owned(),
                        name: required_str(item, "name")?.to_owned(),
                        arguments: serde_json::from_str(required_str(item, "arguments")?)
                            .map_err(|_| "invalid function arguments")?,
                    }),
                    Some(kind) => return Err(format!("unsupported Responses output item: {kind}")),
                    None => return Err("output item missing type".into()),
                }
            }
            (parts, body.get("usage"))
        }
        Format::Claude => {
            let mut parts = Vec::new();
            for item in body
                .get("content")
                .and_then(Value::as_array)
                .ok_or("Claude content missing")?
            {
                match item.get("type").and_then(Value::as_str) {
                    Some("text") => parts.push(Part::Text(required_str(item, "text")?.to_owned())),
                    Some("tool_use") => parts.push(Part::Call {
                        id: required_str(item, "id")?.to_owned(),
                        name: required_str(item, "name")?.to_owned(),
                        arguments: item.get("input").cloned().ok_or("tool_use missing input")?,
                    }),
                    Some(kind) => return Err(format!("unsupported Claude output block: {kind}")),
                    None => return Err("output block missing type".into()),
                }
            }
            (parts, body.get("usage"))
        }
        Format::Gemini => {
            let mut parts = Vec::new();
            for item in body
                .pointer("/candidates/0/content/parts")
                .and_then(Value::as_array)
                .ok_or("Gemini candidate parts missing")?
            {
                if let Some(s) = item.get("text").and_then(Value::as_str) {
                    only_keys(item, &["text"], "Gemini output text part")?;
                    parts.push(Part::Text(s.to_owned()));
                } else if let Some(call) = item.get("functionCall") {
                    only_keys(item, &["functionCall"], "Gemini output function part")?;
                    parts.push(Part::Call {
                        id: required_str(call, "name")?.to_owned(),
                        name: required_str(call, "name")?.to_owned(),
                        arguments: call.get("args").cloned().unwrap_or_else(|| json!({})),
                    });
                } else {
                    return Err("unsupported Gemini output part".into());
                }
            }
            (parts, body.get("usageMetadata"))
        }
    };
    let mut text = String::new();
    let mut calls = Vec::new();
    for part in parts {
        match part {
            Part::Text(s) => text.push_str(&s),
            Part::Call {
                id,
                name,
                arguments,
            } => calls.push((id, name, arguments)),
            Part::Result { .. } => return Err("unexpected output tool result".into()),
        }
    }
    let mut out = match target {
        Format::Chat => {
            let mut message = json!({"role":"assistant","content":text});
            if !calls.is_empty() {
                message["tool_calls"] = json!(calls.iter().map(|(id, name, args)| json!({"id":id,"type":"function","function":{"name":name,"arguments":args.to_string()}})).collect::<Vec<_>>());
            }
            json!({"id":format!("chatcmpl-{}", uuid::Uuid::new_v4()),"object":"chat.completion","model":model,"choices":[{"index":0,"message":message,"finish_reason":if truncated { "length" } else if calls.is_empty() { "stop" } else { "tool_calls" }}]})
        }
        Format::Responses => {
            let mut output = Vec::new();
            if !text.is_empty() {
                output.push(json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":text}]}));
            }
            output.extend(calls.iter().map(|(id, name, args)| json!({"type":"function_call","call_id":id,"name":name,"arguments":args.to_string()})));
            let mut response = json!({"id":format!("resp_{}", uuid::Uuid::new_v4()),"object":"response","status":if truncated { "incomplete" } else { "completed" },"model":model,"output":output});
            if truncated {
                response["incomplete_details"] = json!({"reason":"max_output_tokens"});
            }
            response
        }
        Format::Claude => {
            let mut content = Vec::new();
            if !text.is_empty() {
                content.push(json!({"type":"text","text":text}));
            }
            content.extend(calls.iter().map(
                |(id, name, args)| json!({"type":"tool_use","id":id,"name":name,"input":args}),
            ));
            json!({"id":format!("msg_{}", uuid::Uuid::new_v4()),"type":"message","role":"assistant","model":model,"content":content,"stop_reason":if truncated { "max_tokens" } else if calls.is_empty() { "end_turn" } else { "tool_use" }})
        }
        Format::Gemini => {
            let mut parts = Vec::new();
            if !text.is_empty() {
                parts.push(json!({"text":text}));
            }
            parts.extend(
                calls
                    .iter()
                    .map(|(_, name, args)| json!({"functionCall":{"name":name,"args":args}})),
            );
            json!({"candidates":[{"content":{"role":"model","parts":parts},"finishReason":if truncated { "MAX_TOKENS" } else { "STOP" }}],"modelVersion":model})
        }
    };
    if let Some(u) = usage {
        let input = u
            .get(if source == Format::Gemini {
                "promptTokenCount"
            } else if source == Format::Chat || source == Format::Responses {
                "prompt_tokens"
            } else {
                "input_tokens"
            })
            .or_else(|| u.get("input_tokens"))
            .and_then(Value::as_u64);
        let output = u
            .get(if source == Format::Gemini {
                "candidatesTokenCount"
            } else if source == Format::Chat || source == Format::Responses {
                "completion_tokens"
            } else {
                "output_tokens"
            })
            .or_else(|| u.get("output_tokens"))
            .and_then(Value::as_u64);
        if let (Some(input), Some(output)) = (input, output) {
            match target {
                Format::Chat => {
                    out["usage"] = json!({"prompt_tokens":input,"completion_tokens":output,"total_tokens":input+output})
                }
                Format::Responses | Format::Claude => {
                    out["usage"] = json!({"input_tokens":input,"output_tokens":output})
                }
                Format::Gemini => {
                    out["usageMetadata"] = json!({"promptTokenCount":input,"candidatesTokenCount":output,"totalTokenCount":input+output})
                }
            }
        }
    }
    Ok(out)
}
