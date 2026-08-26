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
    const RELEASE_ACTION: &str =
        "softprops/action-gh-release@3bb12739c298aeb8a4eeaf626c5b8d85266b0e65";
    const BEADS_RUST_COMMIT: &str = "a3f89e6624661259ffa73f876d105656c5b5246e";
    const RELEASE_TARGETS: [&str; 6] = [
        "x86_64-unknown-linux-gnu",
        "x86_64-unknown-linux-musl",
        "aarch64-unknown-linux-gnu",
        "x86_64-apple-darwin",
        "aarch64-apple-darwin",
        "x86_64-pc-windows-msvc",
    ];
    const RELEASE_ARCHIVES: [&str; 6] = [
        "mcp-agent-mail-x86_64-unknown-linux-gnu.tar.xz",
        "mcp-agent-mail-x86_64-unknown-linux-musl.tar.xz",
        "mcp-agent-mail-aarch64-unknown-linux-gnu.tar.xz",
        "mcp-agent-mail-x86_64-apple-darwin.tar.xz",
        "mcp-agent-mail-aarch64-apple-darwin.tar.xz",
        "mcp-agent-mail-x86_64-pc-windows-msvc.zip",
    ];

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
        if action_count != 13 {
            return Err(format!("expected 13 pinned actions, found {action_count}"));
        }
        Ok(())
    }

    fn validate(workflow: &str) -> Result<(), String> {
        validate_action_pins(workflow)?;

        if workflow.contains("sidecar_name=\"${sidecar_name#") {
            return Err("checksum sidecar names must not be normalized".to_string());
        }
        for forbidden in [
            "workflow_dispatch",
            "continue-on-error",
            "No install.sh found",
            "No install.ps1 found",
            "sigstore/cosign-installer@",
            "cosign-release:",
            "sigstore/cosign/releases/latest",
            "curl --insecure",
            "cosign sign-blob",
            "cosign verify-blob",
            "--certificate-identity-regexp",
            "--certificate-oidc-issuer-regexp",
            "--insecure-ignore-sct",
            "--insecure-ignore-tlog",
            "--new-bundle-format=false",
            "SIGSTORE_ROOT_FILE:",
            "SIGSTORE_REKOR_PUBLIC_KEY:",
            "SIGSTORE_CT_LOG_PUBLIC_KEY_FILE:",
            "overwrite_files: true",
            "--method DELETE",
            "deleteRelease",
            "|| true",
            "set +e",
            concat!("mas", "ter"),
        ] {
            if workflow.contains(forbidden) {
                return Err(format!("forbidden release bypass remains: {forbidden}"));
            }
        }

        for (action, expected) in [
            (CHECKOUT_ACTION, 5),
            (TOOLCHAIN_ACTION, 3),
            (UPLOAD_ACTION, 2),
            (DOWNLOAD_ACTION, 2),
            (RELEASE_ACTION, 1),
        ] {
            require_exactly(workflow, action, expected)?;
        }
        require_once(
            workflow,
            "uses: softprops/action-gh-release@3bb12739c298aeb8a4eeaf626c5b8d85266b0e65 # v2.6.2",
        )?;

        let exact_matrix = concat!(
            "        include:\n",
            "          - os: ubuntu-latest\n",
            "            target: x86_64-unknown-linux-gnu\n",
            "          # Statically-linked musl build — runs on any x86_64 Linux regardless\n",
            "          # of host glibc (Debian 12, Ubuntu 22.04, RHEL 9, Amazon Linux 2023,\n",
            "          # Alpine, etc.). Keeps the gnu artifact for distros that prefer it.\n",
            "          - os: ubuntu-latest\n",
            "            target: x86_64-unknown-linux-musl\n",
            "          - os: ubuntu-24.04-arm\n",
            "            target: aarch64-unknown-linux-gnu\n",
            "          - os: macos-15-intel\n",
            "            target: x86_64-apple-darwin\n",
            "          - os: macos-15\n",
            "            target: aarch64-apple-darwin\n",
            "          - os: windows-latest\n",
            "            target: x86_64-pc-windows-msvc",
        );
        require_once(workflow, exact_matrix)?;
        require_exactly(workflow, "            target: ", RELEASE_TARGETS.len())?;
        for target in RELEASE_TARGETS {
            require_once(workflow, &format!("            target: {target}"))?;
        }

        let exact_archive_array = concat!(
            "          expected_archives=(\n",
            "            mcp-agent-mail-x86_64-unknown-linux-gnu.tar.xz\n",
            "            mcp-agent-mail-x86_64-unknown-linux-musl.tar.xz\n",
            "            mcp-agent-mail-aarch64-unknown-linux-gnu.tar.xz\n",
            "            mcp-agent-mail-x86_64-apple-darwin.tar.xz\n",
            "            mcp-agent-mail-aarch64-apple-darwin.tar.xz\n",
            "            mcp-agent-mail-x86_64-pc-windows-msvc.zip\n",
            "          )",
        );
        require_exactly(workflow, exact_archive_array, 2)?;
        for archive in RELEASE_ARCHIVES {
            require_exactly(workflow, archive, 2)?;
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
            "cargo metadata --locked --no-deps --format-version 1 >/dev/null",
            "cargo check --locked --workspace --all-targets",
            "cargo clippy --locked --workspace --all-targets -- -D warnings",
            "cargo test --locked --workspace",
            "cargo build --locked --release --target ${{ matrix.target }}",
            "cli_version=\"$(staging/am --version)\"",
            "server_version=\"$(staging/mcp-agent-mail --version)\"",
            "$cliVersion -ne \"am $env:EXPECTED_VERSION\"",
            "$serverVersion -ne \"mcp-agent-mail $env:EXPECTED_VERSION\"",
            "[System.IO.File]::WriteAllText(",
            "\"$hash  $zipName`n\"",
            "expected_download_entries+=(\"$artifact\" \"${artifact}.sha256\")",
            "mapfile -t actual_download_entries < <(find dist -mindepth 1 -maxdepth 1 -printf '%f\\n' | sort)",
            "[ \"${actual_download_entries[*]}\" != \"${expected_download_entries[*]}\" ]",
            "mapfile -t sidecar_lines < \"dist/${artifact}.sha256\"",
            "[ \"${#sidecar_lines[@]}\" -ne 1 ]",
            "[ \"$sidecar_name\" != \"$artifact\" ]",
            "[ \"$actual_hash\" != \"$sidecar_hash\" ]",
            "cp -- \"dist/$artifact\" \"dist/${artifact}.sha256\" publish/",
            "cp -- install.sh install.ps1 publish/",
            "shasum -a 256 \"${expected_payloads[@]}\" > SHA256SUMS",
            "[ \"${#sums_lines[@]}\" -ne \"${#expected_payloads[@]}\" ]",
            "'$2 == payload && NF == 2 {print $1}'",
            "[ \"${#sums_hashes[@]}\" -ne 1 ]",
            "[ \"$actual_hash\" != \"${sums_hashes[0]}\" ]",
            "names = sorted(member.name for member in members)",
            "names != [\"am\", \"mcp-agent-mail\"]",
            "any(not member.isfile() or member.size <= 0 for member in members)",
            "names = sorted(member.filename for member in members)",
            "names != [\"am.exe\", \"mcp-agent-mail.exe\"]",
            "member.is_dir() or member.file_size <= 0 or stat.S_IFMT(mode) not in (0, stat.S_IFREG)",
            "expected_workflow_ref=\"${EXPECTED_REPOSITORY}/.github/workflows/dist.yml@refs/tags/${RELEASE_TAG}\"",
            "[ \"$GITHUB_WORKFLOW_REF_VALUE\" != \"$expected_workflow_ref\" ]",
            "expected_certificate_identity=\"https://github.com/${expected_workflow_ref}\"",
            "\"$COSIGN_BIN\" sign-blob --yes --bundle \"${subject}.sigstore.json\" \"$subject\"",
            "mapfile -t actual_release_assets < <(find . -mindepth 1 -maxdepth 1 -printf '%f\\n' | sort)",
            "[ \"$checked_out_revision\" != \"$EXPECTED_REVISION\" ] || [ \"$remote_revision\" != \"$EXPECTED_REVISION\" ]",
            "Release tag moved after preflight; refusing publication",
            "path: publish/*",
            "if-no-files-found: error",
            "retention-days: 1",
            "compression-level: 0",
            "mapfile -t actual_release_assets < <(find publish -mindepth 1 -maxdepth 1 -printf '%f\\n' | sort)",
            "[ ! -f \"publish/$asset\" ] || [ -L \"publish/$asset\" ] || [ ! -s \"publish/$asset\" ]",
            "expected_certificate_identity=\"https://github.com/${EXPECTED_REPOSITORY}/.github/workflows/dist.yml@refs/tags/${RELEASE_TAG}\"",
            "list_matching_releases() {",
            "verify_existing_assets_are_matching_subset() {",
            "case \"$release_count\" in",
            "[ \"${#expected_names[@]}\" -ne 30 ]",
            "[ \"$asset_count\" -gt 30 ]",
            "local -a expected_names=() seen_names=()",
            "asset_is_expected=false",
            "[ \"$asset_name\" = \"$expected_name\" ]",
            "asset_is_duplicate=false",
            "[ \"$asset_name\" = \"$seen_name\" ]",
            "[ \"$asset_is_expected\" != true ] || [ \"$asset_is_duplicate\" = true ]",
            "seen_names+=(\"$asset_name\")",
            "Existing draft asset size differs from local ${asset_name}",
            "Existing draft asset bytes differ from local ${asset_name}",
            "'{tag_name: $tag, target_commitish: $revision, name: $tag, draft: true, prerelease: $prerelease, generate_release_notes: true}'",
            "Refusing to mutate a published or metadata-mismatched release for ${RELEASE_TAG}",
            "Refusing ambiguous release state: ${release_count} releases match ${RELEASE_TAG}",
            "Expected exactly one draft after preflight",
            "'.id == $id and .draft == true and .tag_name == $tag and .name == $tag and .prerelease == $prerelease'",
            "direct_release=\"$(gh api \"/repos/${EXPECTED_REPOSITORY}/releases/${release_id}\")\"",
            "Draft id no longer resolves to the isolated release contract",
            "verify_existing_assets_are_matching_subset \"$release_id\"",
            "printf 'release_id=%s\\n' \"$release_id\" >> \"$GITHUB_OUTPUT\"",
            "token: ${{ github.token }}",
            "tag_name: ${{ needs.release_contract.outputs.tag }}",
            "          name: ${{ needs.release_contract.outputs.tag }}",
            "prerelease: ${{ needs.release_contract.outputs.prerelease }}",
            "preserve_order: true",
            "overwrite_files: false",
            "fail_on_unmatched_files: true",
            "files: publish/*",
            "STAGED_RELEASE_ID: ${{ steps.stage_release.outputs.id }}",
            "load_remote_assets() {",
            "assert_release_state_and_census() {",
            "release_json=\"$(gh api \"/repos/${EXPECTED_REPOSITORY}/releases/${EXPECTED_RELEASE_ID}\")\"",
            "'.id == $id and .tag_name == $tag and .name == $tag and .draft == $draft and .prerelease == $prerelease'",
            "[ \"$(jq -r 'length' <<< \"$assets_json\")\" -ne 30 ]",
            "[ \"$actual_names\" != \"$expected_names\" ]",
            "[ \"$STAGED_RELEASE_ID\" != \"$EXPECTED_RELEASE_ID\" ]",
            "draft_assets=\"$(assert_release_state_and_census true)\"",
            "Draft asset bytes differ from local ${asset_name}",
            "gh api --method PATCH \\",
            "-F draft=false)",
            "[ \"$(jq -r '.draft' <<< \"$finalized_release\")\" != false ]",
            "published_assets=\"$(assert_release_state_and_census false)\"",
            "Published asset size differs from local ${asset_name}",
            "Published asset bytes differ from local ${asset_name}",
            "published_by_tag=\"$(gh api \"/repos/${EXPECTED_REPOSITORY}/releases/tags/${RELEASE_TAG}\")\"",
            "'.id == $id and .draft == false and .tag_name == $tag and .name == $tag and .prerelease == $prerelease'",
            "Published release is not discoverable by the expected tag and metadata",
        ];
        for needle in required_once {
            require_once(workflow, needle)?;
        }

        require_exactly(
            workflow,
            "if: ${{ github.event_name == 'push' && startsWith(github.ref, 'refs/tags/v') }}",
            2,
        )?;
        require_exactly(workflow, "set -euo pipefail", 15)?;
        require_exactly(workflow, "contents: read", 2)?;
        require_exactly(workflow, "contents: write", 1)?;
        require_exactly(workflow, "id-token: write", 1)?;
        require_once(
            workflow,
            concat!(
                "  sign:\n",
                "    if: ${{ github.event_name == 'push' && startsWith(github.ref, 'refs/tags/v') }}\n",
                "    needs: [release_contract, lint, test, build]\n",
                "    runs-on: ubuntu-latest\n",
                "    timeout-minutes: 45\n",
                "    permissions:\n",
                "      contents: read\n",
                "      id-token: write\n\n",
                "    steps:",
            ),
        )?;
        require_once(
            workflow,
            concat!(
                "  release:\n",
                "    if: ${{ github.event_name == 'push' && startsWith(github.ref, 'refs/tags/v') }}\n",
                "    needs: [release_contract, sign]\n",
                "    runs-on: ubuntu-latest\n",
                "    timeout-minutes: 60\n",
                "    permissions:\n",
                "      contents: write\n\n",
                "    steps:",
            ),
        )?;
        require_once(
            workflow,
            concat!(
                "      - name: Upload signed release envelope\n",
                "        uses: actions/upload-artifact@ea165f8d65b6e75b540449e92b4886f43607fa02 # v4.6.2\n",
                "        with:\n",
                "          name: signed-release-${{ needs.release_contract.outputs.revision }}\n",
                "          path: publish/*\n",
                "          if-no-files-found: error\n",
                "          retention-days: 1\n",
                "          compression-level: 0",
            ),
        )?;
        require_once(
            workflow,
            concat!(
                "      - name: Download signed release envelope\n",
                "        uses: actions/download-artifact@d3f86a106a0bac45b974a628896c90dbdf5c8093 # v4.3.0\n",
                "        with:\n",
                "          name: signed-release-${{ needs.release_contract.outputs.revision }}\n",
                "          path: publish",
            ),
        )?;
        require_once(
            workflow,
            concat!(
                "      - name: Upload exact assets to draft release\n",
                "        id: stage_release\n",
                "        uses: softprops/action-gh-release@3bb12739c298aeb8a4eeaf626c5b8d85266b0e65 # v2.6.2\n",
                "        with:\n",
                "          token: ${{ github.token }}\n",
                "          tag_name: ${{ needs.release_contract.outputs.tag }}\n",
                "          name: ${{ needs.release_contract.outputs.tag }}\n",
                "          draft: true\n",
                "          prerelease: ${{ needs.release_contract.outputs.prerelease }}\n",
                "          preserve_order: true\n",
                "          overwrite_files: false\n",
                "          fail_on_unmatched_files: true\n",
                "          files: publish/*",
            ),
        )?;
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
        for needle in [
            "- name: Install verified Cosign",
            "COSIGN_VERSION: v3.1.3",
            "COSIGN_LINUX_AMD64_SHA256: 4629c757b7618056f8ddd7e2625ae9fdd94c0372a65049520bc7d9df9efc7f71",
            "curl --fail --location --proto '=https' --proto-redir '=https' --tlsv1.2 \\",
            "https://github.com/sigstore/cosign/releases/download/${COSIGN_VERSION}/cosign-linux-amd64",
            "actual_sha256=\"$(sha256sum \"$cosign_path\" | awk '{print $1}')\"",
            "[ \"$actual_sha256\" != \"$COSIGN_LINUX_AMD64_SHA256\" ]",
            "mapfile -t cosign_versions < <(\"$cosign_path\" version | awk '$1 == \"GitVersion:\" {print $2}')",
            "[ \"${#cosign_versions[@]}\" -ne 1 ] || [ \"${cosign_versions[0]}\" != \"$COSIGN_VERSION\" ]",
            "printf 'COSIGN_BIN=%s\\n' \"$cosign_path\" >> \"$GITHUB_ENV\"",
            "expected_payloads=(install.sh install.ps1)",
            "expected_payloads+=(\"$artifact\" \"${artifact}.sha256\")",
            "signed_subjects=(\"${expected_payloads[@]}\" SHA256SUMS)",
            "\"$COSIGN_BIN\" verify-blob \\",
            "--new-bundle-format \\",
            "--certificate-identity \"$expected_certificate_identity\"",
            "--certificate-oidc-issuer \"https://token.actions.githubusercontent.com\"",
            "--certificate-github-workflow-repository \"$EXPECTED_REPOSITORY\"",
            "--certificate-github-workflow-ref \"refs/tags/${RELEASE_TAG}\"",
            "--certificate-github-workflow-sha \"$EXPECTED_REVISION\"",
            "--certificate-github-workflow-trigger \"push\"",
            "unset SIGSTORE_ROOT_FILE SIGSTORE_REKOR_PUBLIC_KEY SIGSTORE_CT_LOG_PUBLIC_KEY_FILE",
            "expected_release_assets=(\"${signed_subjects[@]}\")",
            "expected_release_assets+=(\"${subject}.sigstore.json\")",
            "[ \"${actual_release_assets[*]}\" != \"${expected_release_assets[*]}\" ]",
            "[ \"${#expected_release_assets[@]}\" -ne 30 ]",
        ] {
            require_exactly(workflow, needle, 2)?;
        }
        require_exactly(workflow, "[ \"$remote_hash\" != \"$local_hash\" ]", 3)?;
        require_exactly(
            workflow,
            "name: signed-release-${{ needs.release_contract.outputs.revision }}",
            2,
        )?;
        require_exactly(workflow, "GH_TOKEN: ${{ github.token }}", 2)?;
        require_exactly(workflow, "SIGSTORE_", 6)?;
        require_exactly(workflow, "assert_tag_revision() {", 2)?;
        require_exactly(
            workflow,
            "gh api \"/repos/${EXPECTED_REPOSITORY}/commits/${RELEASE_TAG}\" --jq '.sha'",
            2,
        )?;
        require_exactly(workflow, "draft: true", 2)?;
        require_exactly(
            workflow,
            "if [[ ! \"$asset_id\" =~ ^[0-9]+$ ]] || [[ ! \"$asset_size\" =~ ^[0-9]+$ ]]; then",
            3,
        )?;
        require_once(
            workflow,
            &format!("BEADS_RUST_COMMIT: {BEADS_RUST_COMMIT}"),
        )?;
        require_exactly(
            workflow,
            "# Cargo.lock resolves beads_rust 0.5.2, so the workspace patch",
            3,
        )?;
        require_exactly(
            workflow,
            "checkout_pinned https://github.com/Dicklesworthstone/beads_rust ../beads_rust \"$BEADS_RUST_COMMIT\"",
            3,
        )?;
        require_in_order(
            workflow,
            &[
                "- name: Install verified Cosign",
                "actual_sha256=\"$(sha256sum \"$cosign_path\" | awk '{print $1}')\"",
                "[ \"$actual_sha256\" != \"$COSIGN_LINUX_AMD64_SHA256\" ]",
                "mapfile -t cosign_versions",
                "[ \"${#cosign_versions[@]}\" -ne 1 ] || [ \"${cosign_versions[0]}\" != \"$COSIGN_VERSION\" ]",
                "printf 'COSIGN_BIN=%s\\n' \"$cosign_path\" >> \"$GITHUB_ENV\"",
                "- name: Assemble, sign, and verify release assets",
                "cp -- install.sh install.ps1 publish/",
                "shasum -a 256 \"${expected_payloads[@]}\" > SHA256SUMS",
                "signed_subjects=(\"${expected_payloads[@]}\" SHA256SUMS)",
                "\"$COSIGN_BIN\" sign-blob --yes --bundle \"${subject}.sigstore.json\" \"$subject\"",
                "\"$COSIGN_BIN\" verify-blob \\",
                "--new-bundle-format \\",
                "expected_release_assets=(\"${signed_subjects[@]}\")",
                "- name: Revalidate release tag immediately before signed handoff",
                "- name: Upload signed release envelope",
                "  release:",
                "- name: Download signed release envelope",
                "- name: Re-census and verify signed release envelope",
                "- name: Revalidate tag and prepare isolated draft",
                "- name: Upload exact assets to draft release",
                "overwrite_files: false",
                "- name: Verify draft bytes, finalize, and verify public census",
                "draft_assets=\"$(assert_release_state_and_census true)\"",
                "gh api --method PATCH \\",
                "published_assets=\"$(assert_release_state_and_census false)\"",
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
                "[System.IO.File]::WriteAllText(\n            \"${zipName}.sha256\",\n            \"$hash  $zipName`n\",\n            [System.Text.Encoding]::ASCII\n          )",
                "\"$hash  $zipName\" | Out-File -Encoding ASCII \"${zipName}.sha256\"",
            ),
            mutate(&workflow, CHECKOUT_ACTION, "actions/checkout@v4"),
            mutate(
                &workflow,
                TOOLCHAIN_ACTION,
                "dtolnay/rust-toolchain@nightly",
            ),
            mutate(
                &workflow,
                RELEASE_ACTION,
                "softprops/action-gh-release@v2.6.2",
            ),
            mutate(
                &workflow,
                "softprops/action-gh-release@3bb12739c298aeb8a4eeaf626c5b8d85266b0e65 # v2.6.2",
                "softprops/action-gh-release@3bb12739c298aeb8a4eeaf626c5b8d85266b0e65 # v2.4.2",
            ),
            mutate(
                &workflow,
                "            target: x86_64-unknown-linux-musl",
                "            target: aarch64-unknown-linux-musl",
            ),
            mutate(
                &workflow,
                "            mcp-agent-mail-x86_64-unknown-linux-musl.tar.xz",
                "            mcp-agent-mail-aarch64-unknown-linux-musl.tar.xz",
            ),
            mutate(&workflow, "set -euo pipefail", "set -eu"),
            mutate(
                &workflow,
                "needs: [release_contract, lint, test, build]",
                "needs: [release_contract, build]",
            ),
            mutate(
                &workflow,
                "needs: [release_contract, sign]",
                "needs: [release_contract, build]",
            ),
            mutate(
                &workflow,
                "permissions:\n      contents: read\n      id-token: write",
                "permissions:\n      contents: write\n      id-token: write",
            ),
            mutate(
                &workflow,
                "permissions:\n      contents: write\n\n    steps:",
                "permissions:\n      contents: write\n      id-token: write\n\n    steps:",
            ),
            mutate(
                &workflow,
                "COSIGN_VERSION: v3.1.3",
                "COSIGN_VERSION: v3.0.2",
            ),
            mutate(
                &workflow,
                "COSIGN_LINUX_AMD64_SHA256: 4629c757b7618056f8ddd7e2625ae9fdd94c0372a65049520bc7d9df9efc7f71",
                "COSIGN_LINUX_AMD64_SHA256: 0000000000000000000000000000000000000000000000000000000000000000",
            ),
            mutate(
                &workflow,
                "https://github.com/sigstore/cosign/releases/download/${COSIGN_VERSION}/cosign-linux-amd64",
                "https://github.com/sigstore/cosign/releases/latest/download/cosign-linux-amd64",
            ),
            mutate(
                &workflow,
                "curl --fail --location --proto '=https' --proto-redir '=https' --tlsv1.2 \\",
                "curl --insecure --location \\",
            ),
            mutate(
                &workflow,
                "[ \"$actual_sha256\" != \"$COSIGN_LINUX_AMD64_SHA256\" ]",
                "[ -z \"$actual_sha256\" ]",
            ),
            mutate(
                &workflow,
                "[ \"${#cosign_versions[@]}\" -ne 1 ] || [ \"${cosign_versions[0]}\" != \"$COSIGN_VERSION\" ]",
                "[ \"${#cosign_versions[@]}\" -eq 0 ]",
            ),
            mutate(
                &workflow,
                BEADS_RUST_COMMIT,
                "b5dc5444270d82218e8de6bb4c6320731e0bdd00",
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
                "read -r sidecar_hash sidecar_name sidecar_extra <<< \"${sidecar_lines[0]}\"",
                "read -r sidecar_hash sidecar_name sidecar_extra <<< \"${sidecar_lines[0]}\"\n            sidecar_name=\"${sidecar_name#\\*}\"",
            ),
            mutate(
                &workflow,
                "[ \"${#sidecar_lines[@]}\" -ne 1 ]",
                "[ \"${#sidecar_lines[@]}\" -eq 0 ]",
            ),
            mutate(
                &workflow,
                "[ \"${actual_download_entries[*]}\" != \"${expected_download_entries[*]}\" ]",
                "[ \"${#actual_download_entries[@]}\" -lt \"${#expected_download_entries[@]}\" ]",
            ),
            mutate(
                &workflow,
                "[ \"$actual_hash\" != \"$sidecar_hash\" ]",
                "[ -z \"$actual_hash\" ]",
            ),
            mutate(
                &workflow,
                "cp -- install.sh install.ps1 publish/",
                "cp -- install.sh publish/",
            ),
            mutate(
                &workflow,
                "cp -- \"dist/$artifact\" \"dist/${artifact}.sha256\" publish/",
                "cp -- \"dist/$artifact\" publish/",
            ),
            mutate(
                &workflow,
                "expected_payloads=(install.sh install.ps1)",
                "expected_payloads=(install.sh)",
            ),
            mutate(
                &workflow,
                "expected_payloads+=(\"$artifact\" \"${artifact}.sha256\")",
                "expected_payloads+=(\"$artifact\")",
            ),
            mutate(
                &workflow,
                "shasum -a 256 \"${expected_payloads[@]}\" > SHA256SUMS",
                "shasum -a 256 \"${expected_archives[@]}\" > SHA256SUMS",
            ),
            mutate(
                &workflow,
                "[ \"${#sums_lines[@]}\" -ne \"${#expected_payloads[@]}\" ]",
                "[ \"${#sums_lines[@]}\" -eq 0 ]",
            ),
            mutate(
                &workflow,
                "[ \"$actual_hash\" != \"${sums_hashes[0]}\" ]",
                "[ -z \"$actual_hash\" ]",
            ),
            mutate(
                &workflow,
                "'$2 == payload && NF == 2 {print $1}'",
                "'$2 == payload || $2 == (\"./\" payload) {print $1}'",
            ),
            mutate(
                &workflow,
                "names = sorted(member.name for member in members)",
                "names = sorted(member.name.removeprefix(\"./\") for member in members)",
            ),
            mutate(
                &workflow,
                "names != [\"am\", \"mcp-agent-mail\"]",
                "names != [\"mcp-agent-mail\"]",
            ),
            mutate(
                &workflow,
                "any(not member.isfile() or member.size <= 0 for member in members)",
                "any(member.size <= 0 for member in members)",
            ),
            mutate(
                &workflow,
                "names = sorted(member.filename for member in members)",
                "names = sorted(member.filename.removeprefix(\"./\") for member in members)",
            ),
            mutate(
                &workflow,
                "names != [\"am.exe\", \"mcp-agent-mail.exe\"]",
                "names != [\"mcp-agent-mail.exe\"]",
            ),
            mutate(
                &workflow,
                "member.is_dir() or member.file_size <= 0 or stat.S_IFMT(mode) not in (0, stat.S_IFREG)",
                "member.is_dir() or member.file_size <= 0",
            ),
            mutate(
                &workflow,
                "signed_subjects=(\"${expected_payloads[@]}\" SHA256SUMS)",
                "signed_subjects=(\"${expected_payloads[@]}\")",
            ),
            mutate(
                &workflow,
                "\"$COSIGN_BIN\" sign-blob --yes --bundle \"${subject}.sigstore.json\" \"$subject\"",
                "\"$COSIGN_BIN\" sign-blob --yes \"$subject\"",
            ),
            mutate(
                &workflow,
                "\"$COSIGN_BIN\" verify-blob \\",
                "true \\",
            ),
            mutate(
                &workflow,
                "--new-bundle-format \\",
                "--new-bundle-format=false \\",
            ),
            mutate(
                &workflow,
                "unset SIGSTORE_ROOT_FILE SIGSTORE_REKOR_PUBLIC_KEY SIGSTORE_CT_LOG_PUBLIC_KEY_FILE",
                "unset SIGSTORE_REKOR_PUBLIC_KEY SIGSTORE_CT_LOG_PUBLIC_KEY_FILE",
            ),
            mutate(
                &workflow,
                "\"$COSIGN_BIN\" sign-blob --yes --bundle \"${subject}.sigstore.json\" \"$subject\"",
                "cosign sign-blob --yes --bundle \"${subject}.sigstore.json\" \"$subject\"",
            ),
            mutate(
                &workflow,
                "--certificate-identity \"$expected_certificate_identity\"",
                "--certificate-identity-regexp \".*\"",
            ),
            mutate(
                &workflow,
                "--certificate-oidc-issuer \"https://token.actions.githubusercontent.com\"",
                "--certificate-oidc-issuer \"https://token.actions.githubusercontent.com\" --insecure-ignore-tlog",
            ),
            mutate(
                &workflow,
                "--certificate-github-workflow-ref \"refs/tags/${RELEASE_TAG}\"",
                "--certificate-github-workflow-ref \"refs/tags/other\"",
            ),
            mutate(
                &workflow,
                "expected_release_assets+=(\"${subject}.sigstore.json\")",
                "expected_release_assets+=(\"$subject\")",
            ),
            mutate(
                &workflow,
                "[ \"${actual_release_assets[*]}\" != \"${expected_release_assets[*]}\" ]",
                "[ \"${#actual_release_assets[@]}\" -lt \"${#expected_release_assets[@]}\" ]",
            ),
            mutate(
                &workflow,
                "[ \"${#expected_release_assets[@]}\" -ne 30 ]",
                "[ \"${#expected_release_assets[@]}\" -lt 30 ]",
            ),
            mutate(
                &workflow,
                "[ ! -f \"publish/$asset\" ] || [ -L \"publish/$asset\" ] || [ ! -s \"publish/$asset\" ]",
                "[ ! -e \"publish/$asset\" ]",
            ),
            mutate(
                &workflow,
                "name: signed-release-${{ needs.release_contract.outputs.revision }}",
                "name: signed-release-${{ github.sha }}",
            ),
            mutate(&workflow, "files: publish/*", "files: dist/*"),
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
            mutate(
                &workflow,
                "gh api \"/repos/${EXPECTED_REPOSITORY}/commits/${RELEASE_TAG}\" --jq '.sha'",
                "gh api \"/repos/${EXPECTED_REPOSITORY}/commits/main\" --jq '.sha'",
            ),
            mutate(
                &workflow,
                "case \"$release_count\" in",
                "case 0 in",
            ),
            mutate(
                &workflow,
                "[ \"$asset_is_expected\" != true ] || [ \"$asset_is_duplicate\" = true ]",
                "[ \"$asset_is_expected\" != true ]",
            ),
            mutate(
                &workflow,
                "'.draft == true and .tag_name == $tag and .name == $tag and .prerelease == $prerelease'",
                "'.tag_name == $tag and .name == $tag and .prerelease == $prerelease'",
            ),
            mutate(
                &workflow,
                "direct_release=\"$(gh api \"/repos/${EXPECTED_REPOSITORY}/releases/${release_id}\")\"",
                "direct_release=\"$(gh api \"/repos/${EXPECTED_REPOSITORY}/releases/tags/${RELEASE_TAG}\")\"",
            ),
            mutate(
                &workflow,
                "release_json=\"$(gh api \"/repos/${EXPECTED_REPOSITORY}/releases/${EXPECTED_RELEASE_ID}\")\"",
                "release_json=\"$(gh api \"/repos/${EXPECTED_REPOSITORY}/releases/tags/${RELEASE_TAG}\")\"",
            ),
            mutate(
                &workflow,
                "[ \"$remote_hash\" != \"$local_hash\" ]",
                "[ -z \"$remote_hash\" ]",
            ),
            mutate(
                &workflow,
                "          draft: true",
                "          draft: false",
            ),
            mutate(
                &workflow,
                "overwrite_files: false",
                "overwrite_files: true",
            ),
            mutate(
                &workflow,
                "'.id == $id and .tag_name == $tag and .name == $tag and .draft == $draft and .prerelease == $prerelease'",
                "'.id == $id and .tag_name == $tag and .draft == $draft and .prerelease == $prerelease'",
            ),
            mutate(
                &workflow,
                "[ \"$(jq -r 'length' <<< \"$assets_json\")\" -ne 30 ]",
                "[ \"$(jq -r 'length' <<< \"$assets_json\")\" -lt 30 ]",
            ),
            mutate(
                &workflow,
                "[ \"$actual_names\" != \"$expected_names\" ]",
                "[ -z \"$actual_names\" ]",
            ),
            mutate(
                &workflow,
                "[ \"$STAGED_RELEASE_ID\" != \"$EXPECTED_RELEASE_ID\" ]",
                "[ -z \"$STAGED_RELEASE_ID\" ]",
            ),
            mutate(
                &workflow,
                "gh api --method PATCH \\",
                "gh api --method DELETE \\",
            ),
            mutate(&workflow, "-F draft=false)", "-F draft=true)"),
            mutate(
                &workflow,
                "published_assets=\"$(assert_release_state_and_census false)\"",
                "published_assets=\"$(assert_release_state_and_census true)\"",
            ),
            mutate(
                &workflow,
                "Published asset size differs from local ${asset_name}",
                "Published asset was not checked against local ${asset_name}",
            ),
            mutate(
                &workflow,
                "Published asset bytes differ from local ${asset_name}",
                "Published asset digest was not checked against local ${asset_name}",
            ),
            mutate(
                &workflow,
                "published_by_tag=\"$(gh api \"/repos/${EXPECTED_REPOSITORY}/releases/tags/${RELEASE_TAG}\")\"",
                "published_by_tag=\"$(gh api \"/repos/${EXPECTED_REPOSITORY}/releases/${EXPECTED_RELEASE_ID}\")\"",
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
