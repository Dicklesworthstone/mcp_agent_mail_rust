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
        {
            return Err("release Dockerfile claims a false arm64 glibc provenance".to_string());
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
