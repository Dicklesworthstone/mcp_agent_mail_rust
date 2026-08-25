#[path = "../crates/mcp-agent-mail-conformance/tests/doc_consistency.rs"]
mod doc_consistency;

#[path = "../crates/mcp-agent-mail-conformance/tests/resource_coverage_guard.rs"]
mod resource_coverage_guard;

mod container_release_contract {
    use std::fs;
    use std::path::{Path, PathBuf};

    fn workspace_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("conformance crate should have a workspace root")
            .to_path_buf()
    }

    fn read(relative: &str) -> String {
        let path = workspace_root().join(relative);
        fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()))
    }

    fn require_exactly_once(text: &str, needle: &str) -> Result<(), String> {
        require_exactly(text, needle, 1)
    }

    fn require_exactly(text: &str, needle: &str, expected: usize) -> Result<(), String> {
        let actual = text.matches(needle).count();
        if actual == expected {
            Ok(())
        } else {
            Err(format!(
                "expected {expected} occurrences of {needle:?}, found {actual}"
            ))
        }
    }

    fn validate(
        workflow: &str,
        release_dockerfile: &str,
        source_dockerfile: &str,
    ) -> Result<(), String> {
        let workflow_once = [
            "dockerfile=\"./Dockerfile.release\"",
            "dockerfile=\"./Dockerfile\"",
            "gh release download \"$RELEASE_TAG\"",
            "actual_api_digest=\"sha256:$(sha256sum",
            "before_fingerprint=\"$(asset_fingerprint",
            "after_fingerprint=\"$(asset_fingerprint",
            "file: ${{ needs.prepare.outputs.dockerfile }}",
            "AM_VERSION=${{ needs.prepare.outputs.version }}",
            "AM_REVISION=${{ needs.prepare.outputs.revision }}",
            "requested_am_ref=\"${INPUT_AM_REF:-main}\"",
            "git ls-remote --heads --tags origin",
            "git fetch --no-tags --depth=1 origin \"$fetch_ref\"",
            "revision=\"$(git rev-parse --verify 'FETCH_HEAD^{commit}')\"",
            "[ \"$revision\" != \"$expected_revision\" ]",
            "type=raw,value=source-${{ steps.refs.outputs.tag_suffix }}-${{ steps.refs.outputs.revision }}",
            "type=raw,value=source-sha-${{ steps.refs.outputs.revision }}",
            "org.opencontainers.image.revision=${{ steps.refs.outputs.revision }}",
            "[ \"$AM_REF\" = \"$REVISION\" ]",
            "grep -Fq 'git fetch --depth 1 origin \"${AM_REF}\"' \"$DOCKERFILE\"",
            "grep -Fq 'git checkout -q FETCH_HEAD' \"$DOCKERFILE\"",
            "provenance: mode=max",
            "expected_digest_files=(linux-amd64.digest linux-arm64.digest)",
            "docker buildx imagetools inspect --raw \"$IMAGE@$digest\"",
        ];
        for needle in workflow_once {
            require_exactly_once(workflow, needle)?;
        }
        require_exactly(
            workflow,
            "actions/upload-artifact@ea165f8d65b6e75b540449e92b4886f43607fa02",
            2,
        )?;
        require_exactly(
            workflow,
            "actions/download-artifact@d3f86a106a0bac45b974a628896c90dbdf5c8093",
            2,
        )?;

        for platform in ["platform: linux/amd64", "platform: linux/arm64"] {
            require_exactly_once(workflow, platform)?;
        }
        require_exactly(workflow, "am_ref=\"$revision\"", 2)?;
        require_exactly(workflow, "Source revision: `%s`", 2)?;
        for asset in [
            "mcp-agent-mail-x86_64-unknown-linux-gnu.tar.xz",
            "mcp-agent-mail-aarch64-unknown-linux-gnu.tar.xz",
        ] {
            require_exactly_once(workflow, asset)?;
        }

        if workflow.contains("file: ./Dockerfile\n") {
            return Err("a hard-coded source Dockerfile publication lane remains".to_string());
        }
        if workflow.contains("value=latest-${{ github.sha }}")
            || workflow.contains("prefix=sha-")
        {
            return Err("source and release tag namespaces can collide".to_string());
        }
        if workflow.contains("source-${{ inputs.tag_suffix }}-${{ github.sha }}")
            || workflow.contains("type=sha,format=long,prefix=source-sha-")
            || workflow.contains("WORKFLOW_SHA: ${{ github.sha }}")
        {
            return Err("manual source identity still depends on the workflow SHA".to_string());
        }

        for needle in [
            "if printf '%s' \"${AM_REF}\" | grep -Eq '^[0-9a-f]{40}$'; then",
            "git fetch --depth 1 origin \"${AM_REF}\";",
            "git checkout -q FETCH_HEAD;",
        ] {
            require_exactly_once(source_dockerfile, needle)?;
        }

        for needle in [
            "ARG AM_VERSION",
            "ARG AM_REVISION",
            "test \"${#AM_REVISION}\" -eq 40",
            "mcp-agent-mail --version)",
            "am --version)",
            "org.opencontainers.image.version=\"${AM_VERSION}\"",
            "org.opencontainers.image.revision=\"${AM_REVISION}\"",
        ] {
            require_exactly_once(release_dockerfile, needle)?;
        }
        require_exactly_once(
            release_dockerfile,
            "The dist matrix builds both GNU artifacts natively",
        )?;
        if release_dockerfile.contains("GLIBC_2.28")
            || release_dockerfile.contains("cargo zigbuild")
            || release_dockerfile.contains("dsr already cross-builds and signs")
        {
            return Err("release Dockerfile claims stale release artifact provenance".to_string());
        }

        Ok(())
    }

    #[test]
    fn release_container_workflow_is_artifact_bound_and_multi_arch() {
        let workflow = read(".github/workflows/docker.yml");
        let release_dockerfile = read("Dockerfile.release");
        let source_dockerfile = read("Dockerfile");
        validate(&workflow, &release_dockerfile, &source_dockerfile)
            .unwrap_or_else(|error| panic!("{error}"));
    }

    #[test]
    fn release_container_contract_guard_rejects_causal_mutations() {
        let workflow = read(".github/workflows/docker.yml");
        let release_dockerfile = read("Dockerfile.release");
        let source_dockerfile = read("Dockerfile");

        let workflow_mutations = [
            workflow.replacen(
                "dockerfile=\"./Dockerfile.release\"",
                "dockerfile=\"./Dockerfile\"",
                1,
            ),
            workflow.replacen(
                "gh release download \"$RELEASE_TAG\"",
                "gh release view \"$RELEASE_TAG\"",
                1,
            ),
            workflow.replacen("platform: linux/arm64", "platform: linux/amd64", 1),
            workflow.replacen(
                "type=raw,value=source-${{ steps.refs.outputs.tag_suffix }}-${{ steps.refs.outputs.revision }}",
                "type=raw,value=source-${{ inputs.tag_suffix }}-${{ github.sha }}",
                1,
            ),
            workflow.replacen(
                "git ls-remote --heads --tags origin",
                "git rev-parse \"$requested_am_ref\"",
                1,
            ),
            workflow.replacen(
                "[ \"$revision\" != \"$expected_revision\" ]",
                "[ -z \"$revision\" ]",
                1,
            ),
            workflow.replacen(
                "org.opencontainers.image.revision=${{ steps.refs.outputs.revision }}",
                "org.opencontainers.image.revision=${{ github.sha }}",
                1,
            ),
            workflow.replacen("am_ref=\"$revision\"", "am_ref=\"$requested_am_ref\"", 2),
            workflow.replacen(
                "[ \"$AM_REF\" = \"$REVISION\" ]",
                "[ -n \"$AM_REF\" ]",
                1,
            ),
            workflow.replacen(
                "grep -Fq 'git checkout -q FETCH_HEAD' \"$DOCKERFILE\"",
                "grep -Fq 'git checkout -q main' \"$DOCKERFILE\"",
                1,
            ),
            workflow.replacen("provenance: mode=max", "provenance: true", 1),
            workflow.replacen(
                "expected_digest_files=(linux-amd64.digest linux-arm64.digest)",
                "expected_digest_files=(linux-amd64.digest)",
                1,
            ),
            workflow.replacen(
                "actions/download-artifact@d3f86a106a0bac45b974a628896c90dbdf5c8093",
                "actions/download-artifact@v4",
                1,
            ),
        ];
        for mutation in workflow_mutations {
            assert!(
                validate(&mutation, &release_dockerfile, &source_dockerfile).is_err(),
                "workflow contract mutation unexpectedly passed"
            );
        }

        let release_dockerfile_mutations = [
            release_dockerfile.replacen("ARG AM_REVISION", "ARG SOURCE_REF", 1),
            release_dockerfile.replacen(
                "mcp-agent-mail --version)",
                "mcp-agent-mail --help)",
                1,
            ),
            release_dockerfile.replacen(
                "test \"${#AM_REVISION}\" -eq 40",
                "test -n \"${AM_REVISION}\"",
                1,
            ),
            release_dockerfile.replacen(
                "The dist matrix builds both GNU artifacts natively",
                "linux/arm64 needs GLIBC_2.28 because cargo zigbuild is used",
                1,
            ),
        ];
        for mutation in release_dockerfile_mutations {
            assert!(
                validate(&workflow, &mutation, &source_dockerfile).is_err(),
                "release Dockerfile contract mutation unexpectedly passed"
            );
        }

        let source_dockerfile_mutation =
            source_dockerfile.replacen("git checkout -q FETCH_HEAD;", "git checkout -q main;", 1);
        assert!(
            validate(&workflow, &release_dockerfile, &source_dockerfile_mutation).is_err(),
            "source Dockerfile checkout mutation unexpectedly passed"
        );
    }
}

mod dist_release_contract {
    use std::fs;
    use std::path::{Path, PathBuf};

    const CHECKOUT_ACTION: &str =
        "actions/checkout@11bd71901bbe5b1630ceea73d27597364c9af683";
    const TOOLCHAIN_ACTION: &str =
        "dtolnay/rust-toolchain@6c977a6ca4077a0ceb28ffbe03f59d46e9ac8772";
    const UPLOAD_ACTION: &str =
        "actions/upload-artifact@ea165f8d65b6e75b540449e92b4886f43607fa02";
    const DOWNLOAD_ACTION: &str =
        "actions/download-artifact@d3f86a106a0bac45b974a628896c90dbdf5c8093";
    const COSIGN_ACTION: &str =
        "sigstore/cosign-installer@faadad0cce49287aee09b3a48701e75088a2c6ad";
    const RELEASE_ACTION: &str =
        "softprops/action-gh-release@5be0e66d93ac7ed76da52eca8bb058f665c3a5fe";

    fn workspace_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("conformance crate should have a workspace root")
            .to_path_buf()
    }

    fn read_workflow() -> String {
        let path = workspace_root().join(".github/workflows/dist.yml");
        fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()))
    }

    fn require_exactly(text: &str, needle: &str, expected: usize) -> Result<(), String> {
        let actual = text.matches(needle).count();
        if actual == expected {
            Ok(())
        } else {
            Err(format!(
                "expected {expected} occurrences of {needle:?}, found {actual}"
            ))
        }
    }

    fn require_once(text: &str, needle: &str) -> Result<(), String> {
        require_exactly(text, needle, 1)
    }

    fn require_in_order(text: &str, needles: &[&str]) -> Result<(), String> {
        let mut remainder = text;
        for needle in needles {
            let Some(index) = remainder.find(needle) else {
                return Err(format!("required ordered marker is missing: {needle:?}"));
            };
            remainder = &remainder[index + needle.len()..];
        }
        Ok(())
    }

    fn validate_action_pins(workflow: &str) -> Result<(), String> {
        let mut action_count = 0;
        for (line_index, line) in workflow.lines().enumerate() {
            let trimmed = line.trim();
            let Some(uses) = trimmed
                .strip_prefix("- uses: ")
                .or_else(|| trimmed.strip_prefix("uses: "))
            else {
                continue;
            };
            action_count += 1;
            let Some((action, comment)) = uses.split_once(" # ") else {
                return Err(format!(
                    "action on line {} lacks a human-readable pin comment",
                    line_index + 1
                ));
            };
            let Some((_, revision)) = action.rsplit_once('@') else {
                return Err(format!("action on line {} lacks a revision", line_index + 1));
            };
            if revision.len() != 40
                || !revision
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            {
                return Err(format!(
                    "action on line {} is not pinned to a lowercase 40-hex commit",
                    line_index + 1
                ));
            }
            if comment.trim().is_empty() {
                return Err(format!("action on line {} has an empty pin comment", line_index + 1));
            }
        }
        if action_count != 12 {
            return Err(format!("expected 12 pinned actions, found {action_count}"));
        }
        Ok(())
    }

    fn validate(workflow: &str) -> Result<(), String> {
        validate_action_pins(workflow)?;

        if workflow.contains("workflow_dispatch") {
            return Err("dist publication must not be dispatchable from a branch".to_string());
        }
        if workflow.contains("continue-on-error") {
            return Err("release gates must not continue after errors".to_string());
        }

        for (action, expected) in [
            (CHECKOUT_ACTION, 5),
            (TOOLCHAIN_ACTION, 3),
            (UPLOAD_ACTION, 1),
            (DOWNLOAD_ACTION, 1),
            (COSIGN_ACTION, 1),
            (RELEASE_ACTION, 1),
        ] {
            require_exactly(workflow, action, expected)?;
        }

        let required_once = [
            "permissions:\n  contents: read",
            "tags:\n      - 'v*'",
            "release_tag_pattern='^v[0-9]+\\.[0-9]+\\.[0-9]+(-[0-9A-Za-z][0-9A-Za-z.-]*)?$'",
            "[ \"$GITHUB_REF_VALUE\" != \"refs/tags/${REF_NAME}\" ]",
            "git ls-remote origin \"refs/tags/${REF_NAME}^{}\"",
            "[ \"$remote_revision\" != \"$revision\" ]",
            "manifest[\"workspace\"][\"package\"][\"version\"]",
            "toolchain[\"toolchain\"][\"channel\"]",
            "tag_version=\"${REF_NAME#v}\"",
            "[ \"$tag_version\" != \"$manifest_version\" ]",
            "if [[ \"$tag_version\" == *-* ]]; then",
            "if: ${{ github.event_name == 'push' && startsWith(github.ref, 'refs/tags/v') }}",
            "cargo metadata --locked --no-deps --format-version 1 >/dev/null",
            "cargo check --locked --workspace --all-targets",
            "cargo clippy --locked --workspace --all-targets -- -D warnings",
            "cargo test --locked --workspace",
            "cargo build --locked --release --target ${{ matrix.target }}",
            "cli_version=\"$(staging/am --version)\"",
            "server_version=\"$(staging/mcp-agent-mail --version)\"",
            "$cliVersion -ne \"am $env:EXPECTED_VERSION\"",
            "$serverVersion -ne \"mcp-agent-mail $env:EXPECTED_VERSION\"",
            "mapfile -t sidecar_lines < \"${artifact}.sha256\"",
            "[ \"${#sidecar_lines[@]}\" -ne 1 ]",
            "[ \"$sidecar_name\" != \"$artifact\" ]",
            "[ \"${#sums_hashes[@]}\" -ne 1 ]",
            "names != [\"am\", \"mcp-agent-mail\"]",
            "names != [\"am.exe\", \"mcp-agent-mail.exe\"]",
            "expected_bundle_files=(SHA256SUMS)",
            "tag_name: ${{ needs.release_contract.outputs.tag }}",
            "prerelease: ${{ needs.release_contract.outputs.prerelease }}",
            "fail_on_unmatched_files: true",
            "if [ \"$checked_out_revision\" != \"$EXPECTED_REVISION\" ] || [ \"$remote_revision\" != \"$EXPECTED_REVISION\" ]; then",
            "Release tag moved after preflight; refusing publication",
        ];
        for needle in required_once {
            require_once(workflow, needle)?;
        }

        require_exactly(workflow, "contents: write", 1)?;
        require_exactly(workflow, "id-token: write", 1)?;
        require_exactly(workflow, "persist-credentials: false", 5)?;
        require_exactly(
            workflow,
            "ref: ${{ needs.release_contract.outputs.revision }}",
            4,
        )?;
        require_exactly(
            workflow,
            "toolchain: ${{ needs.release_contract.outputs.toolchain }}",
            3,
        )?;
        require_exactly(workflow, "rustc --version --verbose", 3)?;
        require_exactly(workflow, "cargo --version --verbose", 3)?;
        require_in_order(
            workflow,
            &[
                "- name: Validate release bundle completeness",
                "- name: Revalidate release tag immediately before publication",
                "- name: Create GitHub Release",
            ],
        )?;

        for line in workflow.lines().map(str::trim) {
            if [
                "cargo metadata ",
                "cargo check ",
                "cargo clippy ",
                "cargo test ",
                "cargo build ",
            ]
                .iter()
                .any(|command| line.contains(command))
                && !line.contains("--locked")
            {
                return Err(format!("release Cargo command is not locked: {line}"));
            }
        }

        Ok(())
    }

    fn mutate(workflow: &str, from: &str, to: &str) -> String {
        let mutation = workflow.replacen(from, to, 1);
        assert_ne!(mutation, workflow, "mutation source was absent: {from}");
        mutation
    }

    #[test]
    fn dist_workflow_is_tag_version_toolchain_and_artifact_bound() {
        let workflow = read_workflow();
        validate(&workflow).unwrap_or_else(|error| panic!("{error}"));
    }

    #[test]
    fn dist_contract_guard_rejects_causal_mutations() {
        let workflow = read_workflow();
        let mutations = [
            mutate(&workflow, "on:\n  push:", "on:\n  workflow_dispatch:\n  push:"),
            mutate(
                &workflow,
                "[ \"$GITHUB_REF_VALUE\" != \"refs/tags/${REF_NAME}\" ]",
                "[ \"$GITHUB_REF_VALUE\" != \"refs/heads/${REF_NAME}\" ]",
            ),
            mutate(
                &workflow,
                "[ \"$remote_revision\" != \"$revision\" ]",
                "[ -z \"$remote_revision\" ]",
            ),
            mutate(
                &workflow,
                "[ \"$tag_version\" != \"$manifest_version\" ]",
                "[ -z \"$manifest_version\" ]",
            ),
            mutate(
                &workflow,
                "prerelease: ${{ needs.release_contract.outputs.prerelease }}",
                "prerelease: false",
            ),
            mutate(
                &workflow,
                "cli_version=\"$(staging/am --version)\"",
                "cli_version=\"$(staging/am --help)\"",
            ),
            mutate(
                &workflow,
                "server_version=\"$(staging/mcp-agent-mail --version)\"",
                "server_version=\"$(staging/mcp-agent-mail --help)\"",
            ),
            mutate(
                &workflow,
                CHECKOUT_ACTION,
                "actions/checkout@v4",
            ),
            mutate(
                &workflow,
                TOOLCHAIN_ACTION,
                "dtolnay/rust-toolchain@nightly",
            ),
            mutate(
                &workflow,
                "cargo check --locked --workspace --all-targets",
                "cargo check --workspace --all-targets",
            ),
            mutate(
                &workflow,
                "cargo metadata --locked --no-deps --format-version 1 >/dev/null",
                "cargo metadata --no-deps --format-version 1 >/dev/null",
            ),
            mutate(&workflow, "contents: read", "contents: write"),
            mutate(
                &workflow,
                "persist-credentials: false",
                "persist-credentials: true",
            ),
            mutate(
                &workflow,
                "[ \"$sidecar_name\" != \"$artifact\" ]",
                "[ -z \"$sidecar_name\" ]",
            ),
            mutate(
                &workflow,
                "[ \"${#sidecar_lines[@]}\" -ne 1 ]",
                "[ \"${#sidecar_lines[@]}\" -eq 0 ]",
            ),
            mutate(
                &workflow,
                "names != [\"am\", \"mcp-agent-mail\"]",
                "names != [\"mcp-agent-mail\"]",
            ),
            mutate(
                &workflow,
                "names != [\"am.exe\", \"mcp-agent-mail.exe\"]",
                "names != [\"mcp-agent-mail.exe\"]",
            ),
            mutate(
                &workflow,
                "ref: ${{ needs.release_contract.outputs.revision }}",
                "ref: ${{ github.sha }}",
            ),
            mutate(
                &workflow,
                "if [ \"$checked_out_revision\" != \"$EXPECTED_REVISION\" ] || [ \"$remote_revision\" != \"$EXPECTED_REVISION\" ]; then",
                "if [ -z \"$remote_revision\" ]; then",
            ),
            mutate(
                &workflow,
                "tag_name: ${{ needs.release_contract.outputs.tag }}",
                "tag_name: ${{ github.ref_name }}",
            ),
        ];
        for mutation in mutations {
            assert!(
                validate(&mutation).is_err(),
                "dist workflow contract mutation unexpectedly passed"
            );
        }
    }
}
