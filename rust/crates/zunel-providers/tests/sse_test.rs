use zunel_providers::sse::SseBuffer;

#[test]
fn emits_single_data_event() {
    let mut buf = SseBuffer::new();
    let events = buf.feed(b"data: hello\n\n").unwrap();
    assert_eq!(events, vec![Some("hello".to_string())]);
}

#[test]
fn splits_across_chunks() {
    let mut buf = SseBuffer::new();
    assert!(buf.feed(b"data: part").unwrap().is_empty());
    assert!(buf.feed(b"ial\n").unwrap().is_empty());
    let events = buf.feed(b"\n").unwrap();
    assert_eq!(events, vec![Some("partial".to_string())]);
}

#[test]
fn multiple_events_in_one_chunk() {
    let mut buf = SseBuffer::new();
    let events = buf.feed(b"data: a\n\ndata: b\n\n").unwrap();
    assert_eq!(events, vec![Some("a".to_string()), Some("b".to_string())]);
}

#[test]
fn done_sentinel_emits_none() {
    let mut buf = SseBuffer::new();
    let events = buf.feed(b"data: [DONE]\n\n").unwrap();
    assert_eq!(events, vec![None]);
}

#[test]
fn ignores_comments_and_unknown_fields() {
    let mut buf = SseBuffer::new();
    let events = buf
        .feed(b": keepalive\nevent: foo\ndata: value\n\n")
        .unwrap();
    assert_eq!(events, vec![Some("value".to_string())]);
}

#[test]
fn multiline_data_joins_with_newlines() {
    let mut buf = SseBuffer::new();
    let events = buf.feed(b"data: line1\ndata: line2\n\n").unwrap();
    assert_eq!(events, vec![Some("line1\nline2".to_string())]);
}

#[test]
fn handles_crlf_line_endings() {
    let mut buf = SseBuffer::new();
    let events = buf.feed(b"data: hi\r\n\r\n").unwrap();
    assert_eq!(events, vec![Some("hi".to_string())]);
}

#[test]
fn buffers_partial_utf8_across_chunks() {
    // `€` = 0xE2 0x82 0xAC; `🌍` = 0xF0 0x9F 0x8C 0x8D.
    // When a TCP boundary splits a multi-byte codepoint we must not
    // drop the prefix — the full glyph should round-trip once the
    // continuation bytes arrive.
    let mut buf = SseBuffer::new();
    assert!(buf.feed(b"data: price ").unwrap().is_empty());
    assert!(buf.feed(&[0xE2, 0x82]).unwrap().is_empty());
    assert!(buf.feed(&[0xAC]).unwrap().is_empty());
    assert!(buf.feed(b" ").unwrap().is_empty());
    assert!(buf.feed(&[0xF0, 0x9F, 0x8C, 0x8D]).unwrap().is_empty());
    let events = buf.feed(b"\n\n").unwrap();
    assert_eq!(events, vec![Some("price \u{20ac} \u{1f30d}".to_string())]);
}
