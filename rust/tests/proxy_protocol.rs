use cliproxy_rs::protocol::{Format, convert_request, convert_response};
use serde_json::json;

#[test]
fn converts_supported_generation_options_and_finish_reasons() {
    let chat = json!({"model":"gemini/test","messages":[{"role":"user","content":"hi"}],"temperature":0.3,"top_p":0.8,"max_tokens":12});
    let gemini = convert_request(Format::Chat, Format::Gemini, &chat).unwrap();
    assert_eq!(
        gemini["generationConfig"],
        json!({"temperature":0.3,"topP":0.8,"maxOutputTokens":12})
    );
    let reverse = convert_request(Format::Gemini, Format::Chat, &gemini).unwrap();
    assert_eq!(reverse["max_tokens"], 12);
    assert_eq!(reverse["temperature"], 0.3);
    assert_eq!(reverse["top_p"], 0.8);

    let incomplete = json!({"status":"incomplete","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"partial","annotations":[]}]}]});
    let converted =
        convert_response(Format::Responses, Format::Chat, &incomplete, "gpt-test").unwrap();
    assert_eq!(converted["choices"][0]["finish_reason"], "length");
    assert_eq!(converted["choices"][0]["message"]["content"], "partial");
    let chat_length =
        json!({"choices":[{"finish_reason":"length","message":{"content":"partial"}}]});
    let converted =
        convert_response(Format::Chat, Format::Claude, &chat_length, "claude-test").unwrap();
    assert_eq!(converted["stop_reason"], "max_tokens");
}

#[test]
fn rejects_lossy_or_unsupported_conversions() {
    let without_limit = json!({"messages":[{"role":"user","content":"hi"}]});
    assert!(
        convert_request(Format::Chat, Format::Claude, &without_limit)
            .unwrap_err()
            .contains("max_tokens")
    );
    let tool_result = json!({"messages":[{"role":"tool","tool_call_id":"call_1","content":"ok"}]});
    assert!(
        convert_request(Format::Chat, Format::Gemini, &tool_result)
            .unwrap_err()
            .contains("function name")
    );
    let thought = json!({"candidates":[{"finishReason":"STOP","content":{"parts":[{"text":"hidden","thought":true}]}}]});
    assert!(convert_response(Format::Gemini, Format::Chat, &thought, "gemini-test").is_err());
    let annotated = json!({"status":"completed","output":[{"type":"message","content":[{"type":"output_text","text":"cited","annotations":[{"type":"url_citation"}]}]}]});
    assert!(convert_response(Format::Responses, Format::Chat, &annotated, "gpt-test").is_err());
}
