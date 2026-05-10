use std::collections::BTreeMap;

use chrono::{TimeZone, Utc};
use mcp_agent_mail_server::tui_screens::system_health::{
    GitRefIntegrityProjectSummary, GitRefIntegrityProjectTarget, GitRefSweepDismissalEntry,
    git_ref_integrity_sweep,
};
use mcp_agent_mail_test_helpers::repo::{self, RepoFixture};

fn fixed_now() -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 5, 10, 14, 0, 0)
        .single()
        .expect("valid fixed timestamp")
}

fn target(slug: &str, repo: &RepoFixture) -> GitRefIntegrityProjectTarget {
    GitRefIntegrityProjectTarget::new(slug, repo.path())
}

fn project_map(
    projects: &[GitRefIntegrityProjectSummary],
) -> BTreeMap<&str, &GitRefIntegrityProjectSummary> {
    projects
        .iter()
        .map(|project| (project.slug(), project))
        .collect()
}

fn corrupt_config_repo() -> RepoFixture {
    let fixture = repo::single_commit();
    std::fs::write(fixture.path().join(".git/config"), b"[not valid git config")
        .expect("corrupt git config");
    fixture
}

#[test]
fn t_sweep_full_cycle_covers_all_projects() {
    let proj_a = repo::single_commit();
    let proj_b = repo::single_commit();
    let proj_c = repo::with_orphan_stash_ref();
    let proj_d = repo::with_dangling_branch();
    let targets = vec![
        target("proj-a", &proj_a),
        target("proj-b", &proj_b),
        target("proj-c", &proj_c),
        target("proj-d", &proj_d),
    ];

    let sweep = git_ref_integrity_sweep(&targets, 0, 4, true, 900, false, &[], fixed_now());
    let projects = project_map(sweep.projects());

    assert_eq!(sweep.total_projects(), 4);
    assert_eq!(sweep.projects_scanned(), 4);
    assert_eq!(sweep.total_findings(), 2);
    assert_eq!(sweep.next_cursor_index(), 0);
    assert_eq!(sweep.level_label(), "WARN");
    assert_eq!(projects["proj-a"].finding_count(), 0);
    assert_eq!(projects["proj-b"].finding_count(), 0);
    assert_eq!(projects["proj-c"].safe_to_prune_count(), 1);
    assert_eq!(projects["proj-d"].ask_user_count(), 1);
}

#[test]
fn t_sweep_round_robin_two_cycles() {
    let proj_a = repo::single_commit();
    let proj_b = repo::single_commit();
    let proj_c = repo::with_orphan_stash_ref();
    let proj_d = repo::with_dangling_branch();
    let targets = vec![
        target("proj-a", &proj_a),
        target("proj-b", &proj_b),
        target("proj-c", &proj_c),
        target("proj-d", &proj_d),
    ];

    let first = git_ref_integrity_sweep(&targets, 0, 2, true, 900, false, &[], fixed_now());
    assert_eq!(first.projects_scanned(), 2);
    assert_eq!(first.total_findings(), 0);
    assert_eq!(first.next_cursor_index(), 2);

    let second = git_ref_integrity_sweep(
        &targets,
        first.next_cursor_index(),
        2,
        true,
        900,
        false,
        &[],
        fixed_now(),
    );
    assert_eq!(second.projects_scanned(), 2);
    assert_eq!(second.total_findings(), 2);
    assert_eq!(second.next_cursor_index(), 0);
}

#[test]
fn t_sweep_dismissal_filters_proj_c_orphan_stash() {
    let proj_c = repo::with_orphan_stash_ref();
    let proj_d = repo::with_dangling_branch();
    let targets = vec![target("proj-c", &proj_c), target("proj-d", &proj_d)];
    let dismissals = vec![GitRefSweepDismissalEntry::new("proj-c", "orphan_stash")];

    let sweep = git_ref_integrity_sweep(&targets, 0, 2, true, 900, false, &dismissals, fixed_now());
    let projects = project_map(sweep.projects());

    assert_eq!(sweep.total_findings(), 1);
    assert_eq!(projects["proj-c"].finding_count(), 0);
    assert_eq!(projects["proj-c"].classification_label(), "OK");
    assert_eq!(projects["proj-d"].finding_count(), 1);
    assert_eq!(projects["proj-d"].classification_label(), "WARN");
}

#[test]
fn t_sweep_banner_appears_then_disappears_after_dismissal() {
    let proj_c = repo::with_orphan_stash_ref();
    let proj_d = repo::with_dangling_branch();
    let targets = vec![target("proj-c", &proj_c), target("proj-d", &proj_d)];

    let visible = git_ref_integrity_sweep(&targets, 0, 2, true, 900, false, &[], fixed_now());
    assert!(
        visible
            .banner()
            .is_some_and(|banner| banner.contains("2 orphan refs across 2 projects"))
    );

    let dismissals = vec![
        GitRefSweepDismissalEntry::new("proj-c", "orphan_stash"),
        GitRefSweepDismissalEntry::new("proj-d", "orphan_ref"),
    ];
    let dismissed =
        git_ref_integrity_sweep(&targets, 0, 2, true, 900, false, &dismissals, fixed_now());
    assert_eq!(dismissed.total_findings(), 0);
    assert!(dismissed.banner().is_none());
}

#[test]
fn t_sweep_handles_corrupt_repo_gracefully() {
    let proj_a = repo::single_commit();
    let proj_b = corrupt_config_repo();
    let proj_c = repo::single_commit();
    let targets = vec![
        target("proj-a", &proj_a),
        target("proj-b", &proj_b),
        target("proj-c", &proj_c),
    ];

    let sweep = git_ref_integrity_sweep(&targets, 0, 3, true, 900, false, &[], fixed_now());
    let projects = project_map(sweep.projects());

    assert_eq!(sweep.projects_scanned(), 3);
    assert_eq!(sweep.level_label(), "FAIL");
    assert_eq!(projects["proj-a"].classification_label(), "OK");
    assert_eq!(projects["proj-b"].classification_label(), "FAIL");
    assert!(projects["proj-b"].error().is_some());
    assert_eq!(projects["proj-c"].classification_label(), "OK");
}
