//! Exercise the production bounded entrypoint with real archive ownership.

use super::tests::{entry, fixture};
use super::*;
use std::fs;
use std::sync::{TryLockError, mpsc};
use std::time::Duration;

fn isolated() -> bool {
    const CHILD: &str = "AM_TEST_CANCELLABLE_MESSAGE_CHILD";
    let thread = std::thread::current();
    let name = thread.name().expect("named libtest thread");
    if std::env::var(CHILD).as_deref() == Ok(name) {
        return false;
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env(CHILD, name)
        .output()
        .expect("run isolated message admission test");
    assert!(
        output.status.success()
            && String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed"),
        "isolated {name} failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    true
}

fn bundle_paths(
    archive: &ProjectArchive,
    message: &serde_json::Value,
    recipients: &[String],
) -> Vec<PathBuf> {
    let paths = crate::message_paths_for_bundle(archive, message, "BlueLake", recipients)
        .unwrap()
        .0;
    let mut all = vec![paths.canonical, paths.outbox];
    all.extend(paths.inbox);
    all
}

fn assert_committed(archive: &ProjectArchive, paths: &[PathBuf]) {
    let repo = Repository::open(&archive.repo_root).unwrap();
    let tree = repo.head().unwrap().peel_to_tree().unwrap();
    for path in paths {
        let relative = crate::rel_path_cached(&archive.canonical_repo_root, path).unwrap();
        let entry = tree.get_path(Path::new(&relative)).unwrap();
        assert_eq!(entry.kind(), Some(ObjectType::Blob));
        let blob = repo.find_blob(entry.id()).unwrap();
        assert_eq!(blob.content(), fs::read(path).unwrap().as_slice());
    }
}

#[test]
fn bounded_repair_preserves_exact_bundle_metadata_privacy_and_idempotence() {
    if isolated() {
        return;
    }
    let (_temp, config, archive, mut message, recipients) = fixture();
    message["future_metadata"] = serde_json::json!({"keep": [true, "λ"]});
    let first = reconcile_message_bundle_cancellable(
        &Cx::for_testing(),
        &archive,
        &config,
        entry(&message, &recipients),
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(first.files_created, 4);
    assert!(first.git_commit_needed);
    let paths = bundle_paths(&archive, &message, &recipients);
    for (index, path) in paths.iter().enumerate() {
        let (actual, body) = read_surviving_message(path).unwrap().unwrap();
        let expected = if index < 2 {
            message.clone()
        } else {
            crate::redact_message_bcc_for_inbox(&message)
        };
        assert_eq!(actual, expected);
        assert_eq!(body, entry(&message, &recipients).body_md);
    }
    assert_committed(&archive, &paths);
    let repo = Repository::open(&archive.repo_root).unwrap();
    let head = repo.head().unwrap().target();
    assert_eq!(
        reconcile_message_bundle(&archive, &config, entry(&message, &recipients)).unwrap(),
        ReconcileResult::default(),
    );
    assert_eq!(
        reconcile_message_bundle_cancellable(
            &Cx::for_testing(),
            &archive,
            &config,
            entry(&message, &recipients),
            &AtomicBool::new(false),
        )
        .unwrap(),
        ReconcileResult::default(),
    );
    assert_eq!(repo.head().unwrap().target(), head);
}

#[test]
fn busy_global_fence_defers_before_changing_any_archive_evidence() {
    if isolated() {
        return;
    }
    let (_temp, config, archive, message, recipients) = fixture();
    let paths = bundle_paths(&archive, &message, &recipients);
    let repo = Repository::open(&archive.repo_root).unwrap();
    let head = repo.head().unwrap().target();
    let token_path = repo.path().join(crate::ARCHIVE_EPOCH_FILE_NAME);
    let token = fs::read(&token_path).unwrap();
    let epoch = crate::archive_mutation_epoch();
    let active = crate::archive_mutations_active();
    let (ready_tx, ready_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let owner = std::thread::spawn(move || {
        crate::with_archive_snapshot_publication_fence(|| {
            ready_tx.send(()).unwrap();
            release_rx.recv_timeout(Duration::from_secs(20))
        })
    });
    ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let result = reconcile_message_bundle_cancellable(
        &Cx::for_testing(),
        &archive,
        &config,
        entry(&message, &recipients),
        &AtomicBool::new(false),
    );
    let held_during_return = crate::archive_publication_fence_holder().is_some();
    let observed = (
        crate::archive_mutation_epoch(),
        crate::archive_mutations_active(),
    );
    let observed_token = fs::read(&token_path).unwrap();
    let observed_head = repo.head().unwrap().target();
    let untouched = paths.iter().all(|path| !path.exists());
    let _ = release_tx.send(());
    assert!(
        owner.join().unwrap().is_ok(),
        "waited for the fence owner's timeout"
    );
    assert!(held_during_return);
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("publication fence busy")
    );
    assert_eq!(observed, (epoch, active));
    assert_eq!(observed_token, token);
    assert_eq!(observed_head, head);
    assert!(untouched);
    assert_eq!(
        reconcile_message_bundle_cancellable(
            &Cx::for_testing(),
            &archive,
            &config,
            entry(&message, &recipients),
            &AtomicBool::new(false),
        )
        .unwrap()
        .files_created,
        4,
    );
    assert_committed(&archive, &paths);
}

#[test]
fn a_busy_project_does_not_prevent_another_message_bundle_from_recovering() {
    if isolated() {
        return;
    }
    let (_temp, config, archive, message, recipients) = fixture();
    let other = crate::ensure_archive(&config, "other-project").unwrap();
    let mut other_message = message.clone();
    other_message["project_slug"] = serde_json::json!(other.slug);
    other_message["project"] = serde_json::json!("/test/other-project");
    let paths = bundle_paths(&archive, &message, &recipients);
    let process = crate::archive_process_lock(&archive).unwrap();
    let owner = process.lock().unwrap();
    let refused = reconcile_message_bundle_cancellable(
        &Cx::for_testing(),
        &archive,
        &config,
        entry(&message, &recipients),
        &AtomicBool::new(false),
    );
    assert!(
        refused
            .unwrap_err()
            .to_string()
            .contains("lock budget exhausted")
    );
    assert!(paths.iter().all(|path| !path.exists()));
    let progressed = reconcile_message_bundle_cancellable(
        &Cx::for_testing(),
        &other,
        &config,
        entry(&other_message, &recipients),
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(progressed.files_created, 4);
    assert_committed(&other, &bundle_paths(&other, &other_message, &recipients));
    assert!(matches!(process.try_lock(), Err(TryLockError::WouldBlock)));
    assert!(paths.iter().all(|path| !path.exists()));
    drop(owner);
    assert_eq!(
        reconcile_message_bundle_cancellable(
            &Cx::for_testing(),
            &archive,
            &config,
            entry(&message, &recipients),
            &AtomicBool::new(false),
        )
        .unwrap()
        .files_created,
        4,
    );
    assert_committed(&archive, &paths);
}

#[test]
fn a_held_flock_keeps_its_owner_evidence_and_all_message_destinations() {
    if isolated() {
        return;
    }
    use fs2::FileExt;
    let (_temp, config, archive, message, recipients) = fixture();
    let paths = bundle_paths(&archive, &message, &recipients);
    let lock_path = archive.root.join(".archive.lock");
    let owner_path = archive.root.join(".archive.lock.owner.json");
    fs::write(&lock_path, b"foreign lock evidence").unwrap();
    fs::write(&owner_path, b"foreign owner evidence").unwrap();
    let owner = fs::OpenOptions::new().write(true).open(&lock_path).unwrap();
    owner.try_lock_exclusive().unwrap();
    let result = reconcile_message_bundle_cancellable(
        &Cx::for_testing(),
        &archive,
        &config,
        entry(&message, &recipients),
        &AtomicBool::new(false),
    );
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("lock budget exhausted")
    );
    assert_eq!(fs::read(&lock_path).unwrap(), b"foreign lock evidence");
    assert_eq!(fs::read(&owner_path).unwrap(), b"foreign owner evidence");
    assert!(paths.iter().all(|path| !path.exists()));
    let contender = fs::OpenOptions::new().write(true).open(&lock_path).unwrap();
    assert!(contender.try_lock_exclusive().is_err());
    drop(contender);
    fs2::FileExt::unlock(&owner).unwrap();
    drop(owner);
    assert_eq!(
        reconcile_message_bundle_cancellable(
            &Cx::for_testing(),
            &archive,
            &config,
            entry(&message, &recipients),
            &AtomicBool::new(false),
        )
        .unwrap()
        .files_created,
        4,
    );
    assert_committed(&archive, &paths);
}

#[test]
fn stop_refuses_publication_and_nested_admission_retains_its_native_owner() {
    if isolated() {
        return;
    }
    let (_temp, config, archive, message, recipients) = fixture();
    let cx = Cx::for_testing();
    let stop = AtomicBool::new(true);
    let epoch = crate::archive_mutation_epoch();
    let result = reconcile_message_bundle_cancellable(
        &cx,
        &archive,
        &config,
        entry(&message, &recipients),
        &stop,
    );
    assert!(matches!(result, Err(StorageError::Io(ref error))
        if error.kind() == std::io::ErrorKind::Interrupted));
    assert_eq!(crate::archive_mutation_epoch(), epoch);
    assert!(
        bundle_paths(&archive, &message, &recipients)
            .iter()
            .all(|path| !path.exists())
    );
    stop.store(false, Ordering::Release);
    let outer = crate::ArchiveMutationGuard::begin_at(&archive.repo_root);
    let holder = crate::archive_publication_fence_holder().unwrap();
    assert_eq!(
        reconcile_message_bundle_cancellable(
            &cx,
            &archive,
            &config,
            entry(&message, &recipients),
            &stop,
        )
        .unwrap()
        .files_created,
        4,
    );
    assert_eq!(crate::ARCHIVE_MUTATION_DEPTH.with(std::cell::Cell::get), 1);
    assert_eq!(crate::archive_mutations_active(), 1);
    assert_eq!(
        crate::archive_publication_fence_holder().unwrap().site,
        holder.site
    );
    drop(outer);
    assert_eq!(crate::ARCHIVE_MUTATION_DEPTH.with(std::cell::Cell::get), 0);
    assert_eq!(crate::archive_mutations_active(), 0);
    assert!(crate::archive_publication_fence_holder().is_none());
}

#[test]
fn bounded_admission_does_not_relax_committed_identity_checks() {
    if isolated() {
        return;
    }
    let (_temp, config, archive, mut message, recipients) = fixture();
    reconcile_message_bundle(&archive, &config, entry(&message, &recipients)).unwrap();
    let paths = bundle_paths(&archive, &message, &recipients);
    let original = paths
        .iter()
        .map(|path| fs::read(path).unwrap())
        .collect::<Vec<_>>();
    let mut evidence = Vec::new();
    for (index, path) in paths.iter().enumerate() {
        let saved = config
            .storage_root
            .join(format!("identity-evidence-{index}.md"));
        fs::rename(path, &saved).unwrap();
        evidence.push(saved);
    }
    let repo = Repository::open(&archive.repo_root).unwrap();
    let head = repo.head().unwrap().target();
    message["reply_to"] = serde_json::json!(9);
    let result = reconcile_message_bundle_cancellable(
        &Cx::for_testing(),
        &archive,
        &config,
        entry(&message, &recipients),
        &AtomicBool::new(false),
    );
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("committed archive artifact")
    );
    assert_eq!(repo.head().unwrap().target(), head);
    assert!(paths.iter().all(|path| !path.exists()));
    for (path, bytes) in evidence.iter().zip(original) {
        assert_eq!(fs::read(path).unwrap(), bytes);
    }
}

#[test]
fn attachment_authority_is_required_before_any_cancellable_message_publication() {
    if isolated() {
        return;
    }
    use sha1::{Digest as _, Sha1};
    let (_temp, config, archive, mut message, recipients) = fixture();
    let bytes = b"retain the attachment with the message";
    let relative = "projects/reconcile-project/attachments/files/evidence.bin";
    message["attachments"] = serde_json::json!([{
        "type": "file", "path": relative, "bytes": bytes.len(),
        "sha1": hex::encode(Sha1::digest(bytes)),
    }]);
    let paths = bundle_paths(&archive, &message, &recipients);
    assert!(
        reconcile_message_bundle_cancellable(
            &Cx::for_testing(),
            &archive,
            &config,
            entry(&message, &recipients),
            &AtomicBool::new(false),
        )
        .is_err()
    );
    assert!(paths.iter().all(|path| !path.exists()));
    let attachment = archive.repo_root.join(relative);
    crate::ensure_parent_dir(&attachment).unwrap();
    fs::write(&attachment, bytes).unwrap();
    assert_eq!(
        reconcile_message_bundle_cancellable(
            &Cx::for_testing(),
            &archive,
            &config,
            entry(&message, &recipients),
            &AtomicBool::new(false),
        )
        .unwrap()
        .files_created,
        4,
    );
    assert_committed(&archive, &paths);
    assert_committed(&archive, std::slice::from_ref(&attachment));
    assert_eq!(fs::read(attachment).unwrap(), bytes);
}
