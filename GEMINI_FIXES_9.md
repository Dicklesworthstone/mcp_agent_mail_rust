# Gemini Fixes Report

## Attachment Storage OOM Vulnerability

I identified and fixed a critical Denial of Service (DoS) vulnerability in `crates/mcp-agent-mail-storage/src/lib.rs` affecting the attachment processing pipeline (`store_attachment` and `store_raw_attachment`).

### Root Cause
Both attachment storage functions were eagerly reading the entire file payload into memory using `fs::read(file_path)?` *before* performing any file size validation against the configured `MAX_ATTACHMENT_BYTES` limit (50 MB).

If an agent or a malicious actor provided a path to an extremely large file (e.g., a 10 GB log file or `/dev/zero`), the server would attempt to allocate the full size into a contiguous `Vec<u8>`, resulting in immediate memory exhaustion (OOM), process termination, and a potential restart loop if the message remained in a retry queue.

### Fix
I modified the logic to perform a lightweight `fs::metadata(file_path)` check first. The size limit is now validated against `meta.len()` before any file content is mapped into RAM.

```rust
    // Check size before reading entire file to prevent OOM
    let meta = fs::metadata(file_path)?;
    if meta.len() > MAX_ATTACHMENT_BYTES as u64 {
        return Err(StorageError::InvalidPath(format!(
            "Attachment too large ({} bytes, max {})",
            meta.len(),
            MAX_ATTACHMENT_BYTES,
        )));
    }

    // Safe to read now
    let bytes = fs::read(file_path)?;
```

This ensures that the memory footprint of the attachment pipeline is strictly bounded by `MAX_ATTACHMENT_BYTES`, maintaining system stability even under adversarial or erroneous inputs.
