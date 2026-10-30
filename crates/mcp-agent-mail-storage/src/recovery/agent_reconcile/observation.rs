//! Bounded complete-profile observations without publication admission.
//!
//! A profile that needs any change still takes the ordinary native fence and
//! project locks, then rebuilds this plan from a fresh source. Observations
//! never publish their captured payloads. They inspect only the two managed
//! artifacts, not the archive history or shared index, and do not require a
//! quiet global epoch: another project may hold the publication fence while
//! these exact working/committed bytes remain complete.

use std::path::PathBuf;

use git2::Repository;
use mcp_agent_mail_core::Config;
use serde_json::json;

use super::{
    AgentSource, Artifact, committed_artifact, head_tree, invalid, merge_metadata, read_artifact,
    serialized_metadata, validate_profile, validate_project,
};
use crate::ProjectArchive;

pub(super) struct ProfileTarget {
    pub(super) path: PathBuf,
    pub(super) relative: String,
    pub(super) bytes: Vec<u8>,
    pub(super) working: Option<Artifact>,
    committed: Option<Artifact>,
}

pub(super) struct ProfilePlan {
    archive: ProjectArchive,
    pub(super) repo_root: PathBuf,
    pub(super) name: String,
    pub(super) targets: [ProfileTarget; 2],
}

impl ProfilePlan {
    /// Both observation and guarded publication use identical authority,
    /// lifecycle, extension preservation and secret-stripping rules.
    pub(super) fn read(archive: &ProjectArchive, source: &AgentSource) -> crate::Result<Self> {
        let root = crate::archive_project_root_checked(archive)?;
        let name = source.profile["name"]
            .as_str()
            .ok_or_else(|| invalid("agent source lacks name"))?;
        let profile_path = root.join("agents").join(name).join("profile.json");
        let project_path = root.join("project.json");
        let repo_root = crate::archive_repo_root_checked(archive)?.to_path_buf();
        let repo = Repository::open(&repo_root)?;
        // Pin one immutable tree for the pair; separate HEAD lookups could
        // combine profile and project evidence from different commits.
        let tree = head_tree(&repo)?;
        let read_target = |path: PathBuf, profile| -> crate::Result<ProfileTarget> {
            let relative = crate::rel_path_cached(&archive.canonical_repo_root, &path)?;
            let working = read_artifact(&path)?;
            let committed = committed_artifact(&repo, tree.as_ref(), &relative)?;
            let bytes = expected_bytes(source, working.as_ref(), committed.as_ref(), profile)?;
            Ok(ProfileTarget {
                path,
                relative,
                bytes,
                working,
                committed,
            })
        };
        Ok(Self {
            archive: archive.clone(),
            repo_root,
            name: name.to_owned(),
            targets: [
                read_target(profile_path, true)?,
                read_target(project_path, false)?,
            ],
        })
    }

    pub(super) fn is_complete(&self) -> bool {
        self.targets.iter().all(|target| {
            target
                .working
                .as_ref()
                .is_some_and(|artifact| artifact.bytes == target.bytes)
                && target
                    .committed
                    .as_ref()
                    .is_some_and(|artifact| artifact.bytes == target.bytes)
        })
    }

    /// A fresh live source must still produce the exact observed bytes. This
    /// reuses the normal last_active_ts-only tolerance; retirement, permanent
    /// deregistration and every other authoritative field invalidate a stale
    /// complete observation.
    pub(super) fn matches_source(&self, source: &AgentSource) -> crate::Result<bool> {
        for (index, target) in self.targets.iter().enumerate() {
            if expected_bytes(
                source,
                target.working.as_ref(),
                target.committed.as_ref(),
                index == 0,
            )? != target.bytes
            {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Reobserve actual working bytes and both stored Git payloads after the
    /// source reread. A moved HEAD is harmless only when these same managed
    /// entries still match. No epoch token, quiet index or global fence is
    /// required: the result cannot mutate, remove or publish anything.
    ///
    /// A later source/file change can require the next finite roster round,
    /// just as it can immediately after the existing guarded observation.
    /// The shared index is deliberately untouched; unchanged does not certify
    /// staged state, and captured staged data never enters a future commit.
    pub(super) fn still_complete(&self) -> crate::Result<bool> {
        // Revalidate the original archive handle before opening Git, not just
        // the leaf bytes afterward. Both configured/canonical roots and every
        // project-prefix component must still pass the ownership/path checks.
        let repo_root = crate::archive_repo_root_checked(&self.archive)?;
        let root = crate::archive_project_root_checked(&self.archive)?;
        if repo_root != self.repo_root.as_path()
            || root.join("agents").join(&self.name).join("profile.json") != self.targets[0].path
            || root.join("project.json") != self.targets[1].path
        {
            return Ok(false);
        }
        let repo = Repository::open(repo_root)?;
        let tree = head_tree(&repo)?;
        for target in &self.targets {
            if read_artifact(&target.path)?.is_none_or(|artifact| artifact.bytes != target.bytes)
                || committed_artifact(&repo, tree.as_ref(), &target.relative)?
                    .is_none_or(|artifact| artifact.bytes != target.bytes)
            {
                return Ok(false);
            }
        }
        crate::archive_repo_root_checked(&self.archive)?;
        crate::archive_project_root_checked(&self.archive)?;
        Ok(true)
    }
}

fn expected_bytes(
    source: &AgentSource,
    working: Option<&Artifact>,
    committed: Option<&Artifact>,
    profile: bool,
) -> crate::Result<Vec<u8>> {
    for artifact in [working, committed].into_iter().flatten() {
        if profile {
            validate_profile(source, &artifact.value)?;
        } else {
            validate_project(source, &artifact.value)?;
        }
    }
    let project = json!({ "slug": source.project_slug, "human_key": source.project_key });
    serialized_metadata(&merge_metadata(
        committed,
        working,
        if profile { &source.profile } else { &project },
        profile,
    ))
}

/// Observation failures carry no publication authority and use the established
/// guarded path for diagnosis/retry. In particular, absence never initializes
/// an archive before nonblocking publication admission succeeds.
pub(super) fn complete_profile(config: &Config, source: &AgentSource) -> Option<ProfilePlan> {
    let archive = crate::open_archive(config, &source.project_slug).ok()??;
    let plan = ProfilePlan::read(&archive, source).ok()?;
    plan.is_complete().then_some(plan)
}
