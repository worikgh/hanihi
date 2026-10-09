/// Provide local time
// use crate::tool::Tool;
use serde_json::{Value, json};

pub(crate) fn exec(params: &Value, id: Value) -> Value {
    tracing::debug!("echo: exec");
    let content = params
        .get("arguments")
        .and_then(|arguments| arguments.get("message"))
        .and_then(Value::as_str);
    match content {
        Some(text) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
            "content": [
            {
            "type": "text",
            "text": text
            }
            ],
            "isError": false
            }
        }),
        None => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {
            "code": -32602,
            "message": "Missing required string argument: message"
            }
        }),
    }
}
pub(crate) fn json() -> Value {
    json!({
    "name": "echo",
    "description": "Returns the supplied message unchanged.",
    "inputSchema": {
    "type": "object",
    "properties": {
    "message": {
    "type": "string",
    "description": "The message to echo."
    }
    },
    "required": ["message"],
    "additionalProperties": false
    }
    })
}
