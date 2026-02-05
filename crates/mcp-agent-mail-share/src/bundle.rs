//! Bundle assembly: attachment materialization, database chunking, and scaffolding.
//!
//! Mirrors the Python `share.py` bundle pipeline.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlmodel_core::Value as SqlValue;

use crate::hosting::{self, HostingHint};
use crate::scope::ProjectScopeResult;
use crate::scrub::ScrubSummary;
use crate::{ShareError, ShareResult};

/// Per-attachment entry in the manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachmentItem {
    pub message_id: i64,
    pub mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub original_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bundle_path: Option<String>,
}

/// Attachment bundling statistics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachmentStats {
    pub inline: u64,
    pub copied: u64,
    pub externalized: u64,
    pub missing: u64,
    pub bytes_copied: u64,
}

/// Attachment manifest returned by [`bundle_attachments`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachmentManifest {
    pub stats: AttachmentStats,
    pub config: AttachmentConfig,
    pub items: Vec<AttachmentItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachmentConfig {
    pub inline_threshold: usize,
    pub detach_threshold: usize,
}

/// Chunk manifest when DB is split into pieces.
///
/// Field names and ordering match the legacy Python config exactly.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkManifest {
    pub version: u32,
    pub chunk_size: usize,
    pub chunk_count: usize,
    pub pattern: String,
    pub original_bytes: u64,
    pub threshold_bytes: usize,
}

/// Bundle all attachments from the snapshot into the output directory.
///
/// Processes each message's attachments JSON array, materializing them as:
/// - **inline**: base64 data URI (≤ `inline_threshold`)
/// - **file**: copied to `attachments/<sha256[:2]>/<sha256>.ext` (between thresholds)
/// - **external**: not bundled, marked with original path (≥ `detach_threshold`)
/// - **missing**: source file not found
pub fn bundle_attachments(
    snapshot_path: &Path,
    output_dir: &Path,
    storage_root: &Path,
    inline_threshold: usize,
    detach_threshold: usize,
) -> ShareResult<AttachmentManifest> {
    use base64::Engine;

    let path_str = snapshot_path.display().to_string();
    let conn = sqlmodel_sqlite::SqliteConnection::open_file(&path_str).map_err(|e| {
        ShareError::Sqlite {
            message: format!("cannot open snapshot: {e}"),
        }
    })?;

    let rows = conn
        .query_sync(
            "SELECT id, attachments FROM messages WHERE attachments != '[]' AND attachments != ''",
            &[],
        )
        .map_err(|e| ShareError::Sqlite {
            message: format!("SELECT messages failed: {e}"),
        })?;

    let attachments_dir = output_dir.join("attachments");
    let mut stats = AttachmentStats {
        inline: 0,
        copied: 0,
        externalized: 0,
        missing: 0,
        bytes_copied: 0,
    };
    let mut items = Vec::new();
    // SHA256 -> relative bundle path (for deduplication of identical content)
    let mut dedup_map: HashMap<String, String> = HashMap::new();

    for row in &rows {
        let msg_id: i64 = row.get_named("id").unwrap_or(0);
        let att_json: String = row.get_named("attachments").unwrap_or_default();

        let mut attachments: Vec<Value> = match serde_json::from_str(&att_json) {
            Ok(Value::Array(arr)) => arr,
            _ => continue,
        };

        let mut updated = false;
        for att in &mut attachments {
            let Some(obj) = att.as_object_mut() else {
                continue;
            };

            // Try to resolve the source file path
            let original_path = obj
                .get("path")
                .or_else(|| obj.get("original_path"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());

            let Some(orig_path_str) = &original_path else {
                continue;
            };

            let source_file = resolve_attachment_path(storage_root, orig_path_str);

            let media_type = obj
                .get("media_type")
                .and_then(|v| v.as_str())
                .unwrap_or("application/octet-stream")
                .to_string();

            match source_file {
                Some(source) if source.exists() => {
                    let file_size = source.metadata().map(|m| m.len()).unwrap_or(0) as usize;
                    let content = std::fs::read(&source)?;
                    let sha = hex_sha256(&content);

                    if file_size <= inline_threshold {
                        // Inline as base64 data URI
                        let data_uri = format!(
                            "data:{};base64,{}",
                            media_type,
                            base64::engine::general_purpose::STANDARD.encode(&content)
                        );
                        obj.insert("type".to_string(), Value::String("inline".to_string()));
                        obj.insert("data_uri".to_string(), Value::String(data_uri));
                        obj.insert("sha256".to_string(), Value::String(sha.clone()));
                        obj.insert(
                            "bytes".to_string(),
                            Value::Number(serde_json::Number::from(file_size as u64)),
                        );
                        stats.inline += 1;
                        items.push(AttachmentItem {
                            message_id: msg_id,
                            mode: "inline".to_string(),
                            sha256: Some(sha),
                            media_type: Some(media_type),
                            bytes: Some(file_size as u64),
                            original_path: original_path.clone(),
                            bundle_path: None,
                        });
                        updated = true;
                    } else if file_size >= detach_threshold {
                        // External — too large to bundle
                        obj.insert("type".to_string(), Value::String("external".to_string()));
                        obj.insert("sha256".to_string(), Value::String(sha.clone()));
                        obj.insert(
                            "bytes".to_string(),
                            Value::Number(serde_json::Number::from(file_size as u64)),
                        );
                        obj.insert(
                            "note".to_string(),
                            Value::String(
                                "Requires manual hosting (exceeds bundle threshold).".to_string(),
                            ),
                        );
                        stats.externalized += 1;
                        items.push(AttachmentItem {
                            message_id: msg_id,
                            mode: "external".to_string(),
                            sha256: Some(sha),
                            media_type: Some(media_type),
                            bytes: Some(file_size as u64),
                            original_path: original_path.clone(),
                            bundle_path: None,
                        });
                        updated = true;
                    } else {
                        // Copy to bundle with deduplication
                        let bundle_rel = if let Some(existing) = dedup_map.get(&sha) {
                            // Deduplicate: reuse existing path
                            existing.clone()
                        } else {
                            let ext = source.extension().and_then(|e| e.to_str()).unwrap_or("bin");
                            let subdir = &sha[..2.min(sha.len())];
                            let rel = format!("attachments/{subdir}/{sha}.{ext}");
                            let dest = output_dir.join(&rel);

                            if let Some(parent) = dest.parent() {
                                std::fs::create_dir_all(parent)?;
                            }
                            std::fs::write(&dest, &content)?;
                            stats.bytes_copied += file_size as u64;
                            dedup_map.insert(sha.clone(), rel.clone());
                            rel
                        };

                        obj.insert("type".to_string(), Value::String("file".to_string()));
                        obj.insert("path".to_string(), Value::String(bundle_rel.clone()));
                        obj.insert("sha256".to_string(), Value::String(sha.clone()));
                        obj.insert(
                            "bytes".to_string(),
                            Value::Number(serde_json::Number::from(file_size as u64)),
                        );
                        stats.copied += 1;
                        items.push(AttachmentItem {
                            message_id: msg_id,
                            mode: "file".to_string(),
                            sha256: Some(sha),
                            media_type: Some(media_type),
                            bytes: Some(file_size as u64),
                            original_path: original_path.clone(),
                            bundle_path: Some(bundle_rel),
                        });
                        updated = true;
                    }
                }
                _ => {
                    // Missing
                    obj.insert("type".to_string(), Value::String("missing".to_string()));
                    if let Some(ref p) = original_path {
                        obj.insert("original_path".to_string(), Value::String(p.clone()));
                    }
                    stats.missing += 1;
                    items.push(AttachmentItem {
                        message_id: msg_id,
                        mode: "missing".to_string(),
                        sha256: None,
                        media_type: Some(media_type),
                        bytes: None,
                        original_path: original_path.clone(),
                        bundle_path: None,
                    });
                    updated = true;
                }
            }
        }

        // Write back updated attachments
        if updated {
            let new_json = serde_json::to_string(&attachments).unwrap_or_else(|_| "[]".to_string());
            conn.execute_sync(
                "UPDATE messages SET attachments = ? WHERE id = ?",
                &[SqlValue::Text(new_json), SqlValue::BigInt(msg_id)],
            )
            .map_err(|e| ShareError::Sqlite {
                message: format!("UPDATE attachments failed: {e}"),
            })?;
        }
    }

    // Ensure attachments dir exists even if empty
    let _ = std::fs::create_dir_all(&attachments_dir);

    Ok(AttachmentManifest {
        stats,
        config: AttachmentConfig {
            inline_threshold,
            detach_threshold,
        },
        items,
    })
}

/// Split a large SQLite database into chunks for streaming.
///
/// Returns `None` if the database is smaller than `threshold_bytes`.
pub fn maybe_chunk_database(
    snapshot_path: &Path,
    output_dir: &Path,
    threshold_bytes: usize,
    chunk_bytes: usize,
) -> ShareResult<Option<ChunkManifest>> {
    let file_size = snapshot_path.metadata()?.len();
    if file_size <= threshold_bytes as u64 {
        return Ok(None);
    }

    let chunks_dir = output_dir.join("chunks");
    std::fs::create_dir_all(&chunks_dir)?;

    let data = std::fs::read(snapshot_path)?;
    let mut sha_lines = Vec::new();
    let mut index = 0usize;
    let mut offset = 0usize;

    while offset < data.len() {
        let end = (offset + chunk_bytes).min(data.len());
        let chunk = &data[offset..end];

        let chunk_name = format!("{index:05}.bin");
        let chunk_path = chunks_dir.join(&chunk_name);
        std::fs::write(&chunk_path, chunk)?;

        let hash = hex_sha256(chunk);
        sha_lines.push(format!("{hash}  chunks/{chunk_name}\n"));

        index += 1;
        offset = end;
    }

    // Write checksums
    let sha_path = output_dir.join("chunks.sha256");
    let checksums_text: String = sha_lines.into_iter().collect();
    std::fs::write(&sha_path, &checksums_text)?;

    // Write chunk config (matches legacy Python format exactly)
    let config = ChunkManifest {
        version: 1,
        chunk_size: chunk_bytes,
        chunk_count: index,
        pattern: "chunks/{index:05d}.bin".to_string(),
        original_bytes: file_size,
        threshold_bytes,
    };
    let config_path = output_dir.join("mailbox.sqlite3.config.json");
    std::fs::write(
        &config_path,
        serde_json::to_string_pretty(&config).unwrap_or_default(),
    )?;

    Ok(Some(config))
}

/// Write the bundle scaffolding files: manifest, README, headers, etc.
#[allow(clippy::too_many_arguments)]
pub fn write_bundle_scaffolding(
    output_dir: &Path,
    scope: &ProjectScopeResult,
    scrub_summary: &ScrubSummary,
    attachment_manifest: &AttachmentManifest,
    chunk_manifest: Option<&ChunkManifest>,
    hosting_hints: &[HostingHint],
    fts_enabled: bool,
    db_path_relative: &str,
    db_sha256: &str,
    db_size_bytes: u64,
) -> ShareResult<()> {
    // manifest.json
    let manifest = build_manifest(
        scope,
        scrub_summary,
        attachment_manifest,
        chunk_manifest,
        hosting_hints,
        fts_enabled,
        db_path_relative,
        db_sha256,
        db_size_bytes,
    );
    let manifest_path = output_dir.join("manifest.json");
    std::fs::write(
        &manifest_path,
        serde_json::to_string_pretty(&manifest).unwrap_or_default(),
    )?;

    // README.md
    let readme = generate_readme(scope, scrub_summary);
    std::fs::write(output_dir.join("README.md"), readme)?;

    // HOW_TO_DEPLOY.md
    let deploy = generate_deploy_guide(hosting_hints);
    std::fs::write(output_dir.join("HOW_TO_DEPLOY.md"), deploy)?;

    // index.html (redirect to viewer)
    let index = r#"<!DOCTYPE html>
<html><head><meta http-equiv="refresh" content="0; url=viewer/"></head>
<body><a href="viewer/">Open viewer</a></body></html>"#;
    std::fs::write(output_dir.join("index.html"), index)?;

    // .nojekyll (GitHub Pages)
    std::fs::write(output_dir.join(".nojekyll"), "")?;

    // _headers (Cloudflare/Netlify COOP/COEP)
    let headers = hosting::generate_headers_file();
    std::fs::write(output_dir.join("_headers"), headers)?;

    Ok(())
}

/// Create a deterministic ZIP archive of a directory.
pub fn package_directory_as_zip(source_dir: &Path, destination: &Path) -> ShareResult<PathBuf> {
    use std::io::Read;
    use zip::write::SimpleFileOptions;

    let file = std::fs::File::create(destination)?;
    let mut zip = zip::ZipWriter::new(file);
    let options = SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .compression_level(Some(9));

    // Collect and sort entries for reproducibility
    let mut entries = Vec::new();
    collect_entries(source_dir, source_dir, &mut entries)?;
    entries.sort();

    for relative_path in &entries {
        let full_path = source_dir.join(relative_path);
        if full_path.is_dir() {
            zip.add_directory(format!("{relative_path}/"), options)
                .map_err(|e| ShareError::Io(std::io::Error::other(e.to_string())))?;
        } else {
            zip.start_file(relative_path.clone(), options)
                .map_err(|e| ShareError::Io(std::io::Error::other(e.to_string())))?;
            let mut f = std::fs::File::open(&full_path)?;
            let mut buf = Vec::new();
            f.read_to_end(&mut buf)?;
            zip.write_all(&buf)?;
        }
    }

    zip.finish()
        .map_err(|e| ShareError::Io(std::io::Error::other(e.to_string())))?;
    Ok(destination.to_path_buf())
}

// === Internal helpers ===

fn resolve_attachment_path(storage_root: &Path, path: &str) -> Option<PathBuf> {
    let root = storage_root
        .canonicalize()
        .unwrap_or_else(|_| storage_root.to_path_buf());
    let candidate = if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        root.join(path)
    };
    let canonical = candidate.canonicalize().ok()?;
    if !canonical.starts_with(&root) {
        return None;
    }
    Some(canonical)
}

fn hex_sha256(data: &[u8]) -> String {
    let hash = Sha256::digest(data);
    hex::encode(hash)
}

fn collect_entries(base: &Path, current: &Path, entries: &mut Vec<String>) -> std::io::Result<()> {
    if !current.is_dir() {
        return Ok(());
    }
    for entry in std::fs::read_dir(current)? {
        let entry = entry?;
        let path = entry.path();
        let relative = path
            .strip_prefix(base)
            .unwrap_or(&path)
            .to_string_lossy()
            .to_string();
        if path.is_dir() {
            entries.push(relative.clone());
            collect_entries(base, &path, entries)?;
        } else {
            entries.push(relative);
        }
    }
    Ok(())
}

fn build_manifest(
    scope: &ProjectScopeResult,
    scrub_summary: &ScrubSummary,
    attachment_manifest: &AttachmentManifest,
    chunk_manifest: Option<&ChunkManifest>,
    hosting_hints: &[HostingHint],
    fts_enabled: bool,
    db_path_relative: &str,
    db_sha256: &str,
    db_size_bytes: u64,
) -> Value {
    let now = chrono::Utc::now().to_rfc3339();

    let requested: Vec<Value> = scope
        .identifiers
        .iter()
        .map(|s| Value::String(s.clone()))
        .collect();
    let included: Vec<Value> = scope
        .projects
        .iter()
        .map(|p| {
            serde_json::json!({
                "slug": p.slug,
                "human_key": p.human_key,
            })
        })
        .collect();

    let hosting_detected: Vec<Value> = hosting_hints
        .iter()
        .map(|h| {
            serde_json::json!({
                "id": h.id,
                "title": h.title,
                "summary": h.summary,
                "signals": h.signals,
            })
        })
        .collect();

    serde_json::json!({
        "schema_version": "0.1.0",
        "generated_at": now,
        "exporter_version": env!("CARGO_PKG_VERSION"),
        "database": {
            "path": db_path_relative,
            "size_bytes": db_size_bytes,
            "sha256": db_sha256,
            "chunked": chunk_manifest.is_some(),
            "chunk_manifest": chunk_manifest,
            "fts_enabled": fts_enabled,
        },
        "project_scope": {
            "requested": requested,
            "included": included,
            "removed_count": scope.removed_count,
        },
        "scrub": scrub_summary,
        "attachments": attachment_manifest,
        "hosting": {
            "detected": hosting_detected,
        },
        "export_config": {
            "projects": requested,
            "scrub_preset": scrub_summary.preset,
            "inline_threshold": attachment_manifest.config.inline_threshold,
            "detach_threshold": attachment_manifest.config.detach_threshold,
        },
    })
}

fn generate_readme(scope: &ProjectScopeResult, scrub: &ScrubSummary) -> String {
    let mut readme = String::from("# MCP Agent Mail Export\n\n");
    readme.push_str("## Quick Start\n\n");
    readme.push_str("Open `index.html` to launch the viewer, ");
    readme.push_str("or deploy to a static hosting platform.\n\n");
    readme.push_str("## Contents\n\n");
    readme.push_str(&format!("- Projects: {}\n", scope.projects.len()));
    readme.push_str(&format!("- Scrub preset: {}\n", scrub.preset));
    readme.push_str(&format!("- Secrets replaced: {}\n", scrub.secrets_replaced));
    readme.push_str("\nSee `manifest.json` for full metadata.\n");
    readme.push_str("\nSee `HOW_TO_DEPLOY.md` for deployment instructions.\n");
    readme
}

fn generate_deploy_guide(hints: &[HostingHint]) -> String {
    let mut guide = String::from("# How to Deploy\n\n");

    if hints.is_empty() {
        guide.push_str("No hosting platform detected. Choose one of:\n\n");
        guide.push_str("1. **GitHub Pages** - Push to a `docs/` directory or `gh-pages` branch\n");
        guide.push_str("2. **Cloudflare Pages** - Connect your repo or upload the bundle\n");
        guide.push_str("3. **Netlify** - Drag-and-drop the bundle directory\n");
        guide.push_str("4. **Amazon S3** - Upload to an S3 bucket with CloudFront\n");
    } else {
        guide.push_str("## Detected Platforms\n\n");
        for hint in hints {
            guide.push_str(&format!("### {}\n\n", hint.title));
            guide.push_str(&format!("{}\n\n", hint.summary));
            guide.push_str("**Signals:**\n");
            for signal in &hint.signals {
                guide.push_str(&format!("- {signal}\n"));
            }
            guide.push_str("\n**Steps:**\n");
            for (i, instr) in hint.instructions.iter().enumerate() {
                guide.push_str(&format!("{}. {instr}\n", i + 1));
            }
            guide.push('\n');
        }
    }

    guide.push_str("\n## Cross-Origin Isolation\n\n");
    guide.push_str(
        "The viewer requires Cross-Origin-Opener-Policy and Cross-Origin-Embedder-Policy\n",
    );
    guide.push_str("headers for OPFS and SharedArrayBuffer support. See `_headers` file.\n");
    guide
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_small_db_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("small.sqlite3");
        std::fs::write(&db, vec![0u8; 1024]).unwrap();
        let result =
            maybe_chunk_database(&db, dir.path(), 20 * 1024 * 1024, 4 * 1024 * 1024).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn chunk_at_exact_threshold_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("exact.sqlite3");
        std::fs::write(&db, vec![0u8; 50_000]).unwrap();
        // size == threshold → no chunking (matches legacy `<=`)
        let result = maybe_chunk_database(&db, dir.path(), 50_000, 10_000).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn chunk_one_byte_over_threshold() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("over.sqlite3");
        std::fs::write(&db, vec![0u8; 50_001]).unwrap();
        let out = dir.path().join("out");
        std::fs::create_dir_all(&out).unwrap();
        // size > threshold → chunking triggered
        let result = maybe_chunk_database(&db, &out, 50_000, 30_000).unwrap();
        assert!(result.is_some());
        let manifest = result.unwrap();
        assert_eq!(manifest.chunk_count, 2); // 50001 / 30000 = 1.67 → 2 chunks
        assert_eq!(manifest.version, 1);
        assert_eq!(manifest.pattern, "chunks/{index:05d}.bin");
        assert_eq!(manifest.original_bytes, 50_001);
    }

    #[test]
    fn chunk_large_db() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("large.sqlite3");
        std::fs::write(&db, vec![0u8; 100_000]).unwrap();
        let out = dir.path().join("out");
        std::fs::create_dir_all(&out).unwrap();
        let result = maybe_chunk_database(&db, &out, 50_000, 30_000).unwrap();
        assert!(result.is_some());
        let manifest = result.unwrap();
        assert_eq!(manifest.chunk_count, 4); // 100k / 30k = 3.33 → 4 chunks
        assert_eq!(manifest.version, 1);
        assert_eq!(manifest.chunk_size, 30_000);
        assert_eq!(manifest.original_bytes, 100_000);
        assert_eq!(manifest.threshold_bytes, 50_000);
        assert_eq!(manifest.pattern, "chunks/{index:05d}.bin");
        assert!(out.join("chunks/00000.bin").exists());
        assert!(out.join("chunks/00003.bin").exists());
        assert!(out.join("chunks.sha256").exists());

        // Verify checksums file format matches legacy (chunks/ prefix)
        let checksums = std::fs::read_to_string(out.join("chunks.sha256")).unwrap();
        let lines: Vec<&str> = checksums.lines().collect();
        assert_eq!(lines.len(), 4);
        for line in &lines {
            assert!(line.contains("  chunks/"), "checksum line should have chunks/ prefix: {line}");
            assert!(line.ends_with(".bin"));
        }
    }

    #[test]
    fn chunk_deterministic_across_runs() {
        let dir1 = tempfile::tempdir().unwrap();
        let dir2 = tempfile::tempdir().unwrap();
        let data = vec![0xABu8; 100_000];

        // Run 1
        let db1 = dir1.path().join("db.sqlite3");
        std::fs::write(&db1, &data).unwrap();
        let out1 = dir1.path().join("out");
        std::fs::create_dir_all(&out1).unwrap();
        let m1 = maybe_chunk_database(&db1, &out1, 50_000, 30_000).unwrap().unwrap();

        // Run 2
        let db2 = dir2.path().join("db.sqlite3");
        std::fs::write(&db2, &data).unwrap();
        let out2 = dir2.path().join("out");
        std::fs::create_dir_all(&out2).unwrap();
        let m2 = maybe_chunk_database(&db2, &out2, 50_000, 30_000).unwrap().unwrap();

        // Manifests match
        assert_eq!(m1.chunk_count, m2.chunk_count);
        assert_eq!(m1.original_bytes, m2.original_bytes);

        // Checksums are identical
        let cs1 = std::fs::read_to_string(out1.join("chunks.sha256")).unwrap();
        let cs2 = std::fs::read_to_string(out2.join("chunks.sha256")).unwrap();
        assert_eq!(cs1, cs2, "checksums should be identical for identical inputs");

        // Chunk files are identical
        for i in 0..m1.chunk_count {
            let c1 = std::fs::read(out1.join(format!("chunks/{i:05}.bin"))).unwrap();
            let c2 = std::fs::read(out2.join(format!("chunks/{i:05}.bin"))).unwrap();
            assert_eq!(c1, c2, "chunk {i} should be identical");
        }
    }

    #[test]
    fn chunk_reassembles_to_original() {
        let dir = tempfile::tempdir().unwrap();
        let original = vec![0xCDu8; 100_000];
        let db = dir.path().join("db.sqlite3");
        std::fs::write(&db, &original).unwrap();
        let out = dir.path().join("out");
        std::fs::create_dir_all(&out).unwrap();

        let manifest = maybe_chunk_database(&db, &out, 50_000, 30_000).unwrap().unwrap();

        // Reassemble chunks
        let mut reassembled = Vec::new();
        for i in 0..manifest.chunk_count {
            let chunk = std::fs::read(out.join(format!("chunks/{i:05}.bin"))).unwrap();
            reassembled.extend_from_slice(&chunk);
        }

        assert_eq!(reassembled, original, "reassembled data should match original");
    }

    #[test]
    fn chunk_config_json_matches_legacy_schema() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("db.sqlite3");
        std::fs::write(&db, vec![0u8; 100_000]).unwrap();
        let out = dir.path().join("out");
        std::fs::create_dir_all(&out).unwrap();

        maybe_chunk_database(&db, &out, 50_000, 30_000).unwrap();

        let config_text = std::fs::read_to_string(out.join("mailbox.sqlite3.config.json")).unwrap();
        let config: serde_json::Value = serde_json::from_str(&config_text).unwrap();

        // Verify all legacy fields present
        assert_eq!(config["version"], 1);
        assert_eq!(config["chunk_size"], 30_000);
        assert_eq!(config["chunk_count"], 4);
        assert_eq!(config["pattern"], "chunks/{index:05d}.bin");
        assert_eq!(config["original_bytes"], 100_000);
        assert_eq!(config["threshold_bytes"], 50_000);
    }

    /// Helper to create a DB with attachment entries pointing to storage files.
    fn create_bundle_test_db(dir: &Path, msg_attachments: &[&str]) -> PathBuf {
        let db_path = dir.join("bundle_test.sqlite3");
        let conn =
            sqlmodel_sqlite::SqliteConnection::open_file(&db_path.display().to_string()).unwrap();
        conn.execute_raw(
            "CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT, human_key TEXT, created_at TEXT DEFAULT '')",
        ).unwrap();
        conn.execute_raw(
            "CREATE TABLE agents (id INTEGER PRIMARY KEY, project_id INTEGER, name TEXT, \
             program TEXT DEFAULT '', model TEXT DEFAULT '', task_description TEXT DEFAULT '', \
             inception_ts TEXT DEFAULT '', last_active_ts TEXT DEFAULT '', \
             attachments_policy TEXT DEFAULT 'auto', contact_policy TEXT DEFAULT 'auto')",
        )
        .unwrap();
        conn.execute_raw(
            "CREATE TABLE messages (id INTEGER PRIMARY KEY, project_id INTEGER, sender_id INTEGER, \
             thread_id TEXT, subject TEXT DEFAULT '', body_md TEXT DEFAULT '', \
             importance TEXT DEFAULT 'normal', ack_required INTEGER DEFAULT 0, \
             created_ts TEXT DEFAULT '', attachments TEXT DEFAULT '[]')",
        )
        .unwrap();
        conn.execute_raw("INSERT INTO projects VALUES (1, 'proj', '/test', '')")
            .unwrap();
        conn.execute_raw(
            "INSERT INTO agents VALUES (1, 1, 'Agent1', '', '', '', '', '', 'auto', 'auto')",
        )
        .unwrap();

        for (i, att_json) in msg_attachments.iter().enumerate() {
            let id = i as i64 + 1;
            let escaped = att_json.replace('\'', "''");
            conn.execute_raw(&format!(
                "INSERT INTO messages VALUES ({id}, 1, 1, NULL, 'Msg {id}', 'Body', 'normal', 0, '', '{escaped}')",
            )).unwrap();
        }

        db_path
    }

    #[test]
    fn bundle_deduplicates_identical_files() {
        let dir = tempfile::tempdir().unwrap();
        let storage = dir.path().join("storage");
        std::fs::create_dir_all(&storage).unwrap();

        // Two files with identical content (100 KiB each, above inline threshold)
        let data = vec![0xABu8; 100 * 1024];
        std::fs::write(storage.join("file_a.bin"), &data).unwrap();
        std::fs::write(storage.join("file_b.bin"), &data).unwrap();

        let att_json = r#"[{"type":"file","path":"file_a.bin","media_type":"application/octet-stream"},{"type":"file","path":"file_b.bin","media_type":"application/octet-stream"}]"#;
        let db = create_bundle_test_db(dir.path(), &[att_json]);
        let output = dir.path().join("bundle");
        std::fs::create_dir_all(&output).unwrap();

        let result = bundle_attachments(
            &db,
            &output,
            &storage,
            crate::INLINE_ATTACHMENT_THRESHOLD,
            crate::DETACH_ATTACHMENT_THRESHOLD,
        )
        .unwrap();

        // Both classified as "file" copies
        assert_eq!(result.stats.copied, 2);
        // But bytes_copied only counts once (deduplication)
        assert_eq!(result.stats.bytes_copied, 100 * 1024);

        // Both should reference the same bundle path
        let paths: Vec<&str> = result
            .items
            .iter()
            .filter_map(|i| i.bundle_path.as_deref())
            .collect();
        assert_eq!(paths.len(), 2);
        assert_eq!(paths[0], paths[1], "duplicate files should share same path");
    }

    #[test]
    fn bundle_inline_small_file() {
        let dir = tempfile::tempdir().unwrap();
        let storage = dir.path().join("storage");
        std::fs::create_dir_all(&storage).unwrap();

        // Small file under inline threshold
        std::fs::write(storage.join("tiny.txt"), b"Hello!").unwrap();

        let att_json = r#"[{"type":"file","path":"tiny.txt","media_type":"text/plain"}]"#;
        let db = create_bundle_test_db(dir.path(), &[att_json]);
        let output = dir.path().join("bundle");
        std::fs::create_dir_all(&output).unwrap();

        let result = bundle_attachments(
            &db,
            &output,
            &storage,
            crate::INLINE_ATTACHMENT_THRESHOLD,
            crate::DETACH_ATTACHMENT_THRESHOLD,
        )
        .unwrap();

        assert_eq!(result.stats.inline, 1);
        assert_eq!(result.items[0].mode, "inline");

        // Verify DB was updated with data: URI
        let conn = sqlmodel_sqlite::SqliteConnection::open_file(&db.display().to_string()).unwrap();
        let rows = conn
            .query_sync("SELECT attachments FROM messages WHERE id = 1", &[])
            .unwrap();
        let att: String = rows[0].get_named("attachments").unwrap();
        assert!(att.contains("data:text/plain;base64,"));
    }

    #[test]
    fn bundle_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let storage = dir.path().join("storage");
        std::fs::create_dir_all(&storage).unwrap();

        let att_json =
            r#"[{"type":"file","path":"nonexistent.dat","media_type":"application/octet-stream"}]"#;
        let db = create_bundle_test_db(dir.path(), &[att_json]);
        let output = dir.path().join("bundle");
        std::fs::create_dir_all(&output).unwrap();

        let result = bundle_attachments(
            &db,
            &output,
            &storage,
            crate::INLINE_ATTACHMENT_THRESHOLD,
            crate::DETACH_ATTACHMENT_THRESHOLD,
        )
        .unwrap();

        assert_eq!(result.stats.missing, 1);
        assert_eq!(result.items[0].mode, "missing");
    }

    #[test]
    fn bundle_externalize_large_file() {
        let dir = tempfile::tempdir().unwrap();
        let storage = dir.path().join("storage");
        std::fs::create_dir_all(&storage).unwrap();

        // Use small thresholds for testing (inline=50, detach=100)
        let data = vec![0xFFu8; 200];
        std::fs::write(storage.join("big.dat"), &data).unwrap();

        let att_json =
            r#"[{"type":"file","path":"big.dat","media_type":"application/octet-stream"}]"#;
        let db = create_bundle_test_db(dir.path(), &[att_json]);
        let output = dir.path().join("bundle");
        std::fs::create_dir_all(&output).unwrap();

        let result = bundle_attachments(&db, &output, &storage, 50, 100).unwrap();

        assert_eq!(result.stats.externalized, 1);
        assert_eq!(result.items[0].mode, "external");
    }

    /// Mixed inline + file + external + missing in one message (br-2ei.4.4.2).
    #[test]
    fn rewrite_mixed_attachment_types() {
        let dir = tempfile::tempdir().unwrap();
        let storage = dir.path().join("storage");
        std::fs::create_dir_all(&storage).unwrap();

        // Create files for 3 of 4 types (one will be "missing")
        // Using thresholds: inline=50, detach=150
        std::fs::write(storage.join("tiny.txt"), b"small").unwrap(); // 5 bytes → inline
        std::fs::write(storage.join("medium.bin"), vec![0x42u8; 80]).unwrap(); // 80 bytes → file
        std::fs::write(storage.join("huge.dat"), vec![0xAAu8; 200]).unwrap(); // 200 bytes → external
        // "gone.txt" doesn't exist → missing

        let att_json = r#"[{"type":"file","path":"tiny.txt","media_type":"text/plain"},{"type":"file","path":"medium.bin","media_type":"application/octet-stream"},{"type":"file","path":"huge.dat","media_type":"application/octet-stream"},{"type":"file","path":"gone.txt","media_type":"text/plain"}]"#;
        let db = create_bundle_test_db(dir.path(), &[att_json]);
        let output = dir.path().join("bundle");
        std::fs::create_dir_all(&output).unwrap();

        let result = bundle_attachments(&db, &output, &storage, 50, 150).unwrap();

        assert_eq!(result.stats.inline, 1);
        assert_eq!(result.stats.copied, 1);
        assert_eq!(result.stats.externalized, 1);
        assert_eq!(result.stats.missing, 1);
        assert_eq!(result.items.len(), 4);

        // Verify ordering is preserved
        assert_eq!(result.items[0].mode, "inline");
        assert_eq!(result.items[1].mode, "file");
        assert_eq!(result.items[2].mode, "external");
        assert_eq!(result.items[3].mode, "missing");

        // Verify DB was updated with all 4 types
        let conn =
            sqlmodel_sqlite::SqliteConnection::open_file(&db.display().to_string()).unwrap();
        let rows = conn
            .query_sync("SELECT attachments FROM messages WHERE id = 1", &[])
            .unwrap();
        let att: String = rows[0].get_named("attachments").unwrap();
        let parsed: Vec<Value> = serde_json::from_str(&att).unwrap();
        assert_eq!(parsed.len(), 4);
        assert_eq!(parsed[0]["type"], "inline");
        assert!(att.contains("data:text/plain;base64,"));
        assert_eq!(parsed[1]["type"], "file");
        assert!(parsed[1]["path"].as_str().unwrap().starts_with("attachments/"));
        assert_eq!(parsed[2]["type"], "external");
        assert!(att.contains("Requires manual hosting"));
        assert_eq!(parsed[3]["type"], "missing");

        // Verify bundle file exists for the "file" type
        let file_path = parsed[1]["path"].as_str().unwrap();
        assert!(output.join(file_path).exists());
    }

    /// Malformed JSON attachments are handled gracefully (br-2ei.4.4.2).
    #[test]
    fn rewrite_malformed_json_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let storage = dir.path().join("storage");
        std::fs::create_dir_all(&storage).unwrap();

        // Message 1: malformed JSON, message 2: valid
        std::fs::write(storage.join("valid.txt"), b"ok").unwrap();
        let db = create_bundle_test_db(
            dir.path(),
            &[
                r#"not valid json {"#,
                r#"[{"type":"file","path":"valid.txt","media_type":"text/plain"}]"#,
            ],
        );
        let output = dir.path().join("bundle");
        std::fs::create_dir_all(&output).unwrap();

        let result = bundle_attachments(
            &db,
            &output,
            &storage,
            crate::INLINE_ATTACHMENT_THRESHOLD,
            crate::DETACH_ATTACHMENT_THRESHOLD,
        )
        .unwrap();

        // Only the valid message was processed
        assert_eq!(result.stats.inline, 1);
        assert_eq!(result.items.len(), 1);
    }

    /// Non-file entries (inline, already processed) pass through unchanged (br-2ei.4.4.2).
    #[test]
    fn rewrite_preserves_non_file_entries() {
        let dir = tempfile::tempdir().unwrap();
        let storage = dir.path().join("storage");
        std::fs::create_dir_all(&storage).unwrap();

        // Already-processed inline entry mixed with a new file entry
        std::fs::write(storage.join("new.txt"), b"data").unwrap();
        let att_json = r#"[{"type":"inline","data_uri":"data:text/plain;base64,b2xk","media_type":"text/plain","bytes":3},{"type":"file","path":"new.txt","media_type":"text/plain"}]"#;
        let db = create_bundle_test_db(dir.path(), &[att_json]);
        let output = dir.path().join("bundle");
        std::fs::create_dir_all(&output).unwrap();

        let result = bundle_attachments(
            &db,
            &output,
            &storage,
            crate::INLINE_ATTACHMENT_THRESHOLD,
            crate::DETACH_ATTACHMENT_THRESHOLD,
        )
        .unwrap();

        // Only 1 new inline (the "new.txt"), the existing inline is preserved
        assert_eq!(result.stats.inline, 1);

        // Verify DB: should have 2 entries, first unchanged
        let conn =
            sqlmodel_sqlite::SqliteConnection::open_file(&db.display().to_string()).unwrap();
        let rows = conn
            .query_sync("SELECT attachments FROM messages WHERE id = 1", &[])
            .unwrap();
        let att: String = rows[0].get_named("attachments").unwrap();
        let parsed: Vec<Value> = serde_json::from_str(&att).unwrap();
        assert_eq!(parsed.len(), 2);
        // First entry (pre-existing inline) should keep its original data_uri
        assert_eq!(parsed[0]["data_uri"], "data:text/plain;base64,b2xk");
        // Second entry (new inline) should have been processed
        assert_eq!(parsed[1]["type"], "inline");
    }

    /// References in bundled output resolve to actual files (br-2ei.4.4.2).
    #[test]
    fn rewrite_all_references_resolve() {
        let dir = tempfile::tempdir().unwrap();
        let storage = dir.path().join("storage");
        std::fs::create_dir_all(&storage).unwrap();

        // Create several files
        for i in 0..3 {
            std::fs::write(
                storage.join(format!("file_{i}.bin")),
                vec![(i as u8 + 1); 100 * 1024],
            )
            .unwrap();
        }

        let att_json = r#"[{"type":"file","path":"file_0.bin","media_type":"application/octet-stream"},{"type":"file","path":"file_1.bin","media_type":"application/octet-stream"},{"type":"file","path":"file_2.bin","media_type":"application/octet-stream"}]"#;
        let db = create_bundle_test_db(dir.path(), &[att_json]);
        let output = dir.path().join("bundle");
        std::fs::create_dir_all(&output).unwrap();

        let result = bundle_attachments(
            &db,
            &output,
            &storage,
            crate::INLINE_ATTACHMENT_THRESHOLD,
            crate::DETACH_ATTACHMENT_THRESHOLD,
        )
        .unwrap();

        assert_eq!(result.stats.copied, 3);

        // Every "file" item has a bundle_path that exists
        for item in &result.items {
            if item.mode == "file" {
                let bp = item.bundle_path.as_ref().expect("file should have bundle_path");
                assert!(
                    output.join(bp).exists(),
                    "bundle_path should resolve: {bp}"
                );
            }
        }
    }
}
