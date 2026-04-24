//! Parser tests for rclone's `--use-json-log` output. No external process
//! needed — we feed known lines and verify deserialization.

use navigator_rclone::log::{LogEvent, LogLevel};

#[test]
fn parses_info_line() {
    let line = r#"{"level":"info","msg":"Copied (new)","source":"operations/copy.go:1","object":"foo.txt","objectType":"*local.Object"}"#;
    let ev: LogEvent = serde_json::from_str(line).expect("parse");
    assert_eq!(ev.level, Some(LogLevel::Info));
    assert_eq!(ev.msg, "Copied (new)");
    assert_eq!(ev.object.as_deref(), Some("foo.txt"));
    assert_eq!(ev.object_type.as_deref(), Some("*local.Object"));
}

#[test]
fn parses_notice_with_stats() {
    let line = r#"{"level":"notice","msg":"stats","stats":{"bytes":1024,"totalBytes":4096,"transfers":1,"totalTransfers":2,"speed":512.0,"elapsedTime":2.0,"errors":0}}"#;
    let ev: LogEvent = serde_json::from_str(line).expect("parse");
    let stats = ev.stats.expect("has stats");
    assert_eq!(stats.bytes, 1024);
    assert_eq!(stats.totalBytes, 4096);
    assert_eq!(stats.transfers, 1);
    assert!((stats.speed - 512.0).abs() < 1e-6);
}

#[test]
fn parses_error_line() {
    let line = r#"{"level":"error","msg":"failed to copy: file not found","object":"missing.bin"}"#;
    let ev: LogEvent = serde_json::from_str(line).expect("parse");
    assert_eq!(ev.level, Some(LogLevel::Error));
    assert!(ev.msg.contains("file not found"));
}

#[test]
fn unknown_level_becomes_unknown_variant() {
    let line = r#"{"level":"trace","msg":"hello"}"#;
    let ev: LogEvent = serde_json::from_str(line).expect("parse");
    assert_eq!(ev.level, Some(LogLevel::Unknown));
}

#[test]
fn missing_optional_fields_default() {
    let line = r#"{"msg":"bare message"}"#;
    let ev: LogEvent = serde_json::from_str(line).expect("parse");
    assert!(ev.level.is_none());
    assert!(ev.object.is_none());
    assert!(ev.stats.is_none());
}
