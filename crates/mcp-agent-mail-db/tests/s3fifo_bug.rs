fn main() {
    let mut cache = mcp_agent_mail_db::s3fifo::S3FifoCache::<String, i32>::new(10);
    cache.insert("Agent0".to_string(), 0);
    for _ in 0..3 {
        cache.get_mut(&"Agent0".to_string());
    }
    for i in 1..20 {
        cache.insert(format!("Agent{i}"), i);
    }
    assert!(cache.peek(&"Agent0".to_string()).is_some());
    println!("SUCCESS");
}