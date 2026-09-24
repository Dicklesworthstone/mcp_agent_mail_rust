//! S3-FIFO regression (895317e9): an entry accessed repeatedly while in the
//! small queue must be promoted, not evicted, when newer entries flood the
//! cache. This was written as a `fn main`, which a libtest target never runs;
//! it has been a real test since the integration tests were consolidated
//! (br-kp1in.28).

#[test]
fn frequently_accessed_entry_survives_a_flood_of_new_entries() {
    let mut cache = mcp_agent_mail_db::s3fifo::S3FifoCache::<String, i32>::new(10);
    cache.insert("Agent0".to_string(), 0);
    for _ in 0..3 {
        cache.get_mut(&"Agent0".to_string());
    }
    for i in 1..20 {
        cache.insert(format!("Agent{i}"), i);
    }
    assert!(cache.peek(&"Agent0".to_string()).is_some());
}
