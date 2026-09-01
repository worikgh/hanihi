use std::path::PathBuf;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let session_name = match args.as_slice() {
        [name] => name,
        _ => {
            eprintln!("usage: analyse SESSION_NAME");
            std::process::exit(2);
        }
    };

    let path = PathBuf::from("./working")
        .join("sessions")
        .join(session_name)
        .join("events.jsonl");

    let content = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(e) => {
            eprintln!("error reading {}: {e}", path.display());
            std::process::exit(1);
        }
    };

    for (i, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let value: serde_json::Value = match serde_json::from_str(line) {
            Ok(value) => value,
            Err(e) => {
                eprintln!("error parsing {} line {}: {e}", path.display(), i + 1);
                std::process::exit(1);
            }
        };

        let kind = value.get("kind").and_then(serde_json::Value::as_str);
        let ts = value.get("ts").and_then(serde_json::Value::as_str);
        if let (Some(kind), Some(ts)) = (kind, ts) {
            println!("{kind}\t{ts}");
        }
    }
}
