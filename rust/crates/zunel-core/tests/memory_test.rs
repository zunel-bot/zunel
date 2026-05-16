use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::stream::BoxStream;
use zunel_core::{DreamCursor, DreamService, MemoryStore};
use zunel_providers::{
    ChatMessage, GenerationSettings, LLMProvider, LLMResponse, StreamEvent, ToolSchema, Usage,
};

#[test]
fn memory_store_reads_and_writes_memory_file() {
    let tmp = tempfile::tempdir().unwrap();
    let store = MemoryStore::new(tmp.path().to_path_buf());

    assert_eq!(store.read_memory().unwrap(), "");
    store
        .write_memory("# Memory\n\n- user likes concise updates")
        .unwrap();

    assert_eq!(
        std::fs::read_to_string(tmp.path().join("memory").join("MEMORY.md")).unwrap(),
        "# Memory\n\n- user likes concise updates"
    );
    assert_eq!(
        store.read_memory().unwrap(),
        "# Memory\n\n- user likes concise updates"
    );
}

#[test]
fn memory_store_reads_and_writes_soul_and_user_files() {
    let tmp = tempfile::tempdir().unwrap();
    let store = MemoryStore::new(tmp.path().to_path_buf());

    store.write_soul("be helpful").unwrap();
    store.write_user("Raymond").unwrap();

    assert_eq!(store.read_soul().unwrap(), "be helpful");
    assert_eq!(store.read_user().unwrap(), "Raymond");
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("SOUL.md")).unwrap(),
        "be helpful"
    );
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("USER.md")).unwrap(),
        "Raymond"
    );
}

#[test]
fn memory_store_appends_python_compatible_history_jsonl() {
    let tmp = tempfile::tempdir().unwrap();
    let store = MemoryStore::new(tmp.path().to_path_buf());

    assert_eq!(store.append_history("first").unwrap(), 1);
    assert_eq!(
        store
            .append_history("<think>hidden</think>\nsecond")
            .unwrap(),
        2
    );

    let entries = store.read_unprocessed_history(0).unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].cursor, 1);
    assert_eq!(entries[0].content, "first");
    assert_eq!(entries[1].cursor, 2);
    assert_eq!(entries[1].content, "second");
    assert_eq!(
        store.read_unprocessed_history(1).unwrap()[0].content,
        "second"
    );
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("memory").join(".cursor")).unwrap(),
        "2"
    );
}

#[test]
fn memory_store_compacts_old_history_entries() {
    let tmp = tempfile::tempdir().unwrap();
    let store = MemoryStore::new(tmp.path().to_path_buf()).with_max_history_entries(2);

    store.append_history("one").unwrap();
    store.append_history("two").unwrap();
    store.append_history("three").unwrap();
    store.compact_history().unwrap();

    let entries = store.read_unprocessed_history(0).unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].content, "two");
    assert_eq!(entries[1].content, "three");
}

#[test]
fn dream_cursor_round_trips_processed_offset() {
    let tmp = tempfile::tempdir().unwrap();
    let cursor = DreamCursor::new(tmp.path().to_path_buf());

    assert_eq!(cursor.read().unwrap(), 0);
    cursor.write(42).unwrap();

    assert_eq!(cursor.read().unwrap(), 42);
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("memory").join(".dream_cursor")).unwrap(),
        "42"
    );
}

struct DreamProvider {
    stream_calls: Mutex<usize>,
}

#[async_trait]
impl LLMProvider for DreamProvider {
    async fn generate(
        &self,
        _model: &str,
        messages: &[ChatMessage],
        _tools: &[ToolSchema],
        _settings: &GenerationSettings,
    ) -> zunel_providers::Result<LLMResponse> {
        assert!(messages[1].content.contains("user likes Rust"));
        Ok(LLMResponse {
            content: Some("Add the durable preference to MEMORY.md.".into()),
            tool_calls: Vec::new(),
            usage: Usage::default(),
            finish_reason: None,
        })
    }

    fn generate_stream<'a>(
        &'a self,
        _model: &'a str,
        messages: &'a [ChatMessage],
        tools: &'a [ToolSchema],
        _settings: &'a GenerationSettings,
    ) -> BoxStream<'a, zunel_providers::Result<StreamEvent>> {
        assert!(messages[1].content.contains("Analysis Result"));
        assert!(tools.iter().any(|tool| tool.name == "write_file"));
        let call = {
            let mut guard = self.stream_calls.lock().unwrap();
            let call = *guard;
            *guard += 1;
            call
        };
        Box::pin(async_stream::stream! {
            if call == 0 {
                yield Ok(StreamEvent::ToolCallDelta {
                    index: 0,
                    id: Some("call-write".into()),
                    name: Some("write_file".into()),
                    arguments_fragment: Some(serde_json::json!({
                        "path": "memory/MEMORY.md",
                        "content": "# Memory\n\n- user likes Rust"
                    }).to_string()),
                });
                yield Ok(StreamEvent::Done(LLMResponse {
                    content: None,
                    tool_calls: Vec::new(),
                    usage: Usage::default(),
                    finish_reason: Some("tool_calls".into()),
                }));
            } else {
                yield Ok(StreamEvent::ContentDelta("done".into()));
                yield Ok(StreamEvent::Done(LLMResponse {
                    content: Some("done".into()),
                    tool_calls: Vec::new(),
                    usage: Usage::default(),
                    finish_reason: None,
                }));
            }
        })
    }
}

#[tokio::test]
async fn dream_service_processes_history_updates_memory_and_advances_cursor() {
    let tmp = tempfile::tempdir().unwrap();
    let store = MemoryStore::new(tmp.path().to_path_buf());
    store.append_history("user likes Rust").unwrap();

    let provider = Arc::new(DreamProvider {
        stream_calls: Mutex::new(0),
    });
    let dream = DreamService::new(store, provider, "m".into());

    assert!(dream.run().await.unwrap());
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("memory").join("MEMORY.md")).unwrap(),
        "# Memory\n\n- user likes Rust"
    );
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("memory").join(".dream_cursor")).unwrap(),
        "1"
    );
}
