/// Provide local time
use serde_json::{Value, json};

pub(crate) fn json() -> Value {
    json!({
    "name": "noop",
    "description": "A tool to perform no operation.",
    "inputSchema": {
    "type": "object",
    "properties": {}
    },
    "additionalProperties": false
    }
    )
}

pub(crate) fn exec(_params: &Value, id: Value) -> Value {
    json!({
    "jsonrpc": "2.0",
    "id": id,
    "result": {
        "content": [
        {
            "type": "text",
            "text": "noop completed"
        }
        ],
        "isError": false
    }
    })
}
