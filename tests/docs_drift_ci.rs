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
        if workflow.contains("value=latest-${{ github.sha }}") || workflow.contains("prefix=sha-") {
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
            release_dockerfile.replacen("mcp-agent-mail --version)", "mcp-agent-mail --help)", 1),
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

    const CHECKOUT_ACTION: &str = "actions/checkout@11bd71901bbe5b1630ceea73d27597364c9af683";
    const TOOLCHAIN_ACTION: &str =
        "dtolnay/rust-toolchain@6c977a6ca4077a0ceb28ffbe03f59d46e9ac8772";
    const UPLOAD_ACTION: &str = "actions/upload-artifact@ea165f8d65b6e75b540449e92b4886f43607fa02";
    const DOWNLOAD_ACTION: &str =
        "actions/download-artifact@d3f86a106a0bac45b974a628896c90dbdf5c8093";
    const SETUP_ZIG_ACTION: &str = "mlugg/setup-zig@d1434d08867e3ee9daa34448df10607b98908d29";
    const FRANKENSEARCH_COMMIT: &str = "e2d24118b06e4a1a4bbff16968f70767f35d5bb3";
    const FAST_CMAES_COMMIT: &str = "3bf11b7601b54a08ffabb55821289ecb235a504d";
    const BEADS_RUST_COMMIT: &str = "ba6ff75da25529c8cad8395352f5c5fc2162ee95";
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
                return Err(format!(
                    "action on line {} lacks a revision",
                    line_index + 1
                ));
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
                return Err(format!(
                    "action on line {} has an empty pin comment",
                    line_index + 1
                ));
            }
        }
        if action_count != 14 {
            return Err(format!("expected 14 pinned actions, found {action_count}"));
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
            "softprops/action-gh-release@",
            "overwrite_files:",
            "--show-error --location",
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
            (DOWNLOAD_ACTION, 3),
            (SETUP_ZIG_ACTION, 1),
        ] {
            require_exactly(workflow, action, expected)?;
        }

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
            let expected = if archive.starts_with("mcp-agent-mail-x86_64-unknown-linux-") {
                // The two release manifests plus the portability job's exact census,
                // checksum verification, and extraction command.
                6
            } else {
                2
            };
            require_exactly(workflow, archive, expected)?;
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
            "CARGO_ZIGBUILD_VERSION: '0.23.3'",
            "LINUX_GLIBC_FLOOR: '2.28'",
            "version: 0.14.1",
            "cargo install --locked --version \"$CARGO_ZIGBUILD_VERSION\" cargo-zigbuild",
            "actual_version=\"$(cargo zigbuild --version)\"",
            "cargo zigbuild --locked --release --target \"${target}.${LINUX_GLIBC_FLOOR}\"",
            "cargo build --locked --release --target \"$target\"",
            "  linux_portability:",
            "name: Run x86_64 Linux release archives on Ubuntu 22.04",
            "runs-on: ubuntu-22.04",
            "pattern: x86_64-unknown-linux-*",
            "sha256sum --check mcp-agent-mail-x86_64-unknown-linux-gnu.tar.xz.sha256",
            "sha256sum --check mcp-agent-mail-x86_64-unknown-linux-musl.tar.xz.sha256",
            "getconf GNU_LIBC_VERSION",
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
            "Existing draft asset size differs from local ${asset_name}",
            "Existing draft asset bytes differ from local ${asset_name}",
            "'{tag_name: $tag, target_commitish: $revision, name: $tag, draft: true, prerelease: $prerelease, generate_release_notes: true}'",
            "Refusing to mutate a published or metadata-mismatched release for ${RELEASE_TAG}",
            "Refusing ambiguous release state: ${release_count} releases match ${RELEASE_TAG}",
            "Expected exactly one draft after preflight",
            "direct_release=\"$(gh api \"/repos/${EXPECTED_REPOSITORY}/releases/${release_id}\")\"",
            "Draft id no longer resolves to the isolated release contract",
            "verify_existing_assets_are_matching_subset \"$release_id\"",
            "printf 'release_id=%s\\n' \"$release_id\" >> \"$GITHUB_OUTPUT\"",
            "- name: Upload missing exact assets to isolated draft",
            "assert_isolated_draft() {",
            "expected_upload_url=\"https://uploads.github.com/repos/${EXPECTED_REPOSITORY}/releases/${EXPECTED_RELEASE_ID}/assets{?name,label}\"",
            "Numeric release id no longer resolves to the isolated draft upload contract",
            "validate_remote_subset() {",
            "local verify_bytes=\"$2\"",
            "[ \"$verify_bytes\" != true ] && [ \"$verify_bytes\" != false ]",
            "if [ \"$verify_bytes\" = true ]; then",
            "mapfile -t local_assets < <(find publish -mindepth 1 -maxdepth 1 -printf '%f\\n' | sort)",
            "[ \"${#local_assets[@]}\" -ne 30 ]",
            "declare -A local_sizes=() local_hashes=()",
            "local_sizes[\"$asset_name\"]=\"$(stat --format='%s' \"publish/$asset_name\")\"",
            "local_hashes[\"$asset_name\"]=\"$(sha256sum \"publish/$asset_name\" | awk '{print $1}')\"",
            "Repository name cannot be embedded safely in an upload URL",
            "Release asset name cannot be embedded safely in an upload URL: ${asset_name}",
            "matching_count=\"$(jq -r --arg name \"$asset_name\" '[.[] | select(.name == $name)] | length' <<< \"$staged_assets\")\"",
            "[ \"$matching_count\" -eq 1 ]",
            "[ \"$matching_count\" -ne 0 ]",
            "--connect-timeout 30 --max-time 1800 \\",
            "--request POST \\",
            "-H \"Authorization: Bearer ${GH_TOKEN}\" \\",
            "-H 'Content-Type: application/octet-stream' \\",
            "--data-binary \"@publish/$asset_name\" \\",
            "https://uploads.github.com/repos/${EXPECTED_REPOSITORY}/releases/${EXPECTED_RELEASE_ID}/assets?name=${asset_name}",
            "'(.id | type == \"number\") and .name == $name and .size == $size and .digest == $digest and .state == \"uploaded\"'",
            "GitHub returned an invalid upload receipt for ${asset_name}",
            "validate_remote_subset \"$staged_assets\" true",
            "uploaded_asset_id=\"$(jq -r '.id' <<< \"$upload_response\")\"",
            "[ \"$uploaded_hash\" != \"$local_hash\" ]",
            "Uploaded isolated-draft asset bytes differ from local ${asset_name}",
            "[ \"$(jq -r 'length' <<< \"$staged_assets\")\" -ne 30 ]",
            "printf 'release_id=%s\\n' \"$EXPECTED_RELEASE_ID\" >> \"$GITHUB_OUTPUT\"",
            "STAGED_RELEASE_ID: ${{ steps.stage_release.outputs.release_id }}",
            "assert_release_state_and_census() {",
            "[ \"$(jq -r 'length' <<< \"$assets_json\")\" -ne 30 ]",
            "[ \"$actual_names\" != \"$expected_names\" ]",
            "[ \"$STAGED_RELEASE_ID\" != \"$EXPECTED_RELEASE_ID\" ]",
            "draft_assets=\"$(assert_release_state_and_census true)\"",
            "Draft asset digest or state differs from local ${asset_name}",
            "Draft asset bytes differ from local ${asset_name}",
            "resolve_acfs_public_main() {",
            "resolve_mcp_public_main() {",
            "https://github.com/Dicklesworthstone/agentic_coding_flywheel_setup.git",
            "https://github.com/Dicklesworthstone/mcp_agent_mail_rust.git",
            "-H 'Accept: application/vnd.github.raw+json' \\",
            "Signed installer differs from install.sh at the release revision",
            "if ! cmp --silent \"$tag_installer\" publish/install.sh; then",
            "installer_hash=\"$(sha256sum \"$tag_installer\" | awk '{print $1}')\"",
            "mcp_agent_mail_rust/${mcp_main_revision}/install.sh?cache_bust=${mcp_main_revision}",
            "if ! cmp --silent \"$tag_installer\" \"$main_installer\"; then",
            "Public MCP Agent Mail main does not serve the signed release installer",
            "agentic_coding_flywheel_setup/${acfs_revision}/checksums.yaml?cache_bust=${acfs_revision}",
            "marker = \"  mcp_agent_mail:\"",
            "ACFS checksums must contain exactly one mcp_agent_mail entry",
            "url: \"https://raw.githubusercontent.com/Dicklesworthstone/mcp_agent_mail_rust/refs/heads/main/install.sh\"",
            "sha256: \"{expected_hash}\"",
            "ACFS public main does not bind mcp_agent_mail to this release installer",
            "[ \"$acfs_revision_after\" != \"$acfs_revision\" ]",
            "[ \"$mcp_main_revision_after\" != \"$mcp_main_revision\" ]",
            "ACFS public main moved during checksum verification; refusing publication",
            "MCP Agent Mail public main moved during checksum verification; refusing publication",
            "gh api --method PATCH \\",
            "-F draft=false)",
            "[ \"$(jq -r '.draft' <<< \"$finalized_release\")\" != false ]",
            "published_assets=\"$(assert_release_state_and_census false)\"",
            "Published asset size differs from local ${asset_name}",
            "Published asset digest or state differs from local ${asset_name}",
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
        require_exactly(workflow, "set -euo pipefail", 19)?;
        require_exactly(workflow, "assert_glibc_floor() {", 2)?;
        require_exactly(
            workflow,
            "LANG=C readelf -W --version-info --dyn-syms \"$binary\"",
            2,
        )?;
        require_exactly(workflow, "newer than GLIBC_${LINUX_GLIBC_FLOOR}", 2)?;
        require_exactly(workflow, "contents: read", 2)?;
        require_exactly(workflow, "contents: write", 1)?;
        require_exactly(workflow, "id-token: write", 1)?;
        require_once(
            workflow,
            concat!(
                "  sign:\n",
                "    if: ${{ github.event_name == 'push' && startsWith(github.ref, 'refs/tags/v') }}\n",
                "    needs: [release_contract, lint, test, build, linux_portability]\n",
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
                "      - name: Upload missing exact assets to isolated draft\n",
                "        id: stage_release\n",
                "        env:\n",
                "          EXPECTED_PRERELEASE: ${{ needs.release_contract.outputs.prerelease }}\n",
                "          EXPECTED_RELEASE_ID: ${{ steps.draft_preflight.outputs.release_id }}\n",
                "          EXPECTED_REPOSITORY: ${{ github.repository }}\n",
                "          EXPECTED_REVISION: ${{ needs.release_contract.outputs.revision }}\n",
                "          GH_TOKEN: ${{ github.token }}\n",
                "          RELEASE_TAG: ${{ needs.release_contract.outputs.tag }}\n",
                "        run: |\n",
                "          set -euo pipefail",
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
        require_exactly(workflow, "[ \"$remote_hash\" != \"$local_hash\" ]", 4)?;
        require_exactly(
            workflow,
            "curl --fail --location --proto '=https' --proto-redir '=https' --tlsv1.2 \\",
            4,
        )?;
        require_exactly(
            workflow,
            "name: signed-release-${{ needs.release_contract.outputs.revision }}",
            2,
        )?;
        require_exactly(workflow, "GH_TOKEN: ${{ github.token }}", 3)?;
        require_exactly(workflow, "SIGSTORE_", 6)?;
        require_exactly(workflow, "output=\"$(timeout 30 git ls-remote --refs \\", 2)?;
        require_exactly(workflow, "assert_tag_revision() {", 2)?;
        require_exactly(
            workflow,
            "gh api \"/repos/${EXPECTED_REPOSITORY}/commits/${RELEASE_TAG}\" --jq '.sha'",
            2,
        )?;
        require_exactly(workflow, "draft: true", 1)?;
        require_exactly(
            workflow,
            "if [[ ! \"$asset_id\" =~ ^[0-9]+$ ]] || [[ ! \"$asset_size\" =~ ^[0-9]+$ ]]; then",
            4,
        )?;
        for needle in [
            "load_remote_assets() {",
            "[ \"$asset_count\" -gt 30 ]",
            "asset_is_expected=false",
            "[ \"$asset_name\" = \"$expected_name\" ]",
            "asset_is_duplicate=false",
            "[ \"$asset_name\" = \"$seen_name\" ]",
            "[ \"$asset_is_expected\" != true ] || [ \"$asset_is_duplicate\" = true ]",
            "seen_names+=(\"$asset_name\")",
            "EXPECTED_RELEASE_ID: ${{ steps.draft_preflight.outputs.release_id }}",
            "release_json=\"$(gh api \"/repos/${EXPECTED_REPOSITORY}/releases/${EXPECTED_RELEASE_ID}\")\"",
            "validate_remote_subset \"$staged_assets\" false",
            "local_size=\"${local_sizes[$asset_name]}\"",
            "local_hash=\"${local_hashes[$asset_name]}\"",
        ] {
            require_exactly(workflow, needle, 2)?;
        }
        require_once(workflow, "[ \"${#expected_names[@]}\" -ne 30 ]")?;
        require_once(workflow, "local -a expected_names=() seen_names=()")?;
        require_once(
            workflow,
            "'.draft == true and .tag_name == $tag and .name == $tag and .prerelease == $prerelease'",
        )?;
        require_exactly(
            workflow,
            "'.id == $id and .draft == true and .tag_name == $tag and .name == $tag and .prerelease == $prerelease'",
            2,
        )?;
        require_once(
            workflow,
            "'.id == $id and .draft == true and .tag_name == $tag and .name == $tag and .prerelease == $prerelease and .upload_url == $upload_url'",
        )?;
        require_once(
            workflow,
            "'.id == $id and .tag_name == $tag and .name == $tag and .draft == $draft and .prerelease == $prerelease'",
        )?;
        require_once(
            workflow,
            &format!("FRANKENSEARCH_COMMIT: {FRANKENSEARCH_COMMIT}"),
        )?;
        require_once(workflow, &format!("FAST_CMAES_COMMIT: {FAST_CMAES_COMMIT}"))?;
        require_once(workflow, &format!("BEADS_RUST_COMMIT: {BEADS_RUST_COMMIT}"))?;
        require_exactly(
            workflow,
            "# Cargo.lock resolves beads_rust 0.5.4, so the workspace patch",
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
                "  linux_portability:",
                "sha256sum --check mcp-agent-mail-x86_64-unknown-linux-gnu.tar.xz.sha256",
                "assert_glibc_floor staging/gnu/mcp-agent-mail",
                "getconf GNU_LIBC_VERSION",
                "  sign:",
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
                "- name: Upload missing exact assets to isolated draft",
                "https://uploads.github.com/repos/${EXPECTED_REPOSITORY}/releases/${EXPECTED_RELEASE_ID}/assets?name=${asset_name}",
                "- name: Verify draft bytes, finalize, and verify public census",
                "draft_assets=\"$(assert_release_state_and_census true)\"",
                "tag_installer=\"${RUNNER_TEMP}/tag-install-${EXPECTED_REVISION}.sh\"",
                "mcp_main_revision=\"$(resolve_mcp_public_main)\"",
                "if ! cmp --silent \"$tag_installer\" \"$main_installer\"; then",
                "acfs_revision=\"$(resolve_acfs_public_main)\"",
                "[ \"$acfs_revision_after\" != \"$acfs_revision\" ]",
                "assert_tag_revision\n          finalized_release=\"$(gh api --method PATCH \\",
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
                "cargo zigbuild ",
            ]
            .iter()
            .any(|command| line.contains(command))
                && !line.contains("cargo zigbuild --version")
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
            mutate(
                &workflow,
                "on:\n  push:",
                "on:\n  workflow_dispatch:\n  push:",
            ),
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
                "needs: [release_contract, lint, test, build, linux_portability]",
                "needs: [release_contract, build]",
            ),
            mutate(
                &workflow,
                "LINUX_GLIBC_FLOOR: '2.28'",
                "LINUX_GLIBC_FLOOR: '2.39'",
            ),
            mutate(&workflow, SETUP_ZIG_ACTION, "mlugg/setup-zig@v2"),
            mutate(
                &workflow,
                "cargo zigbuild --locked --release --target \"${target}.${LINUX_GLIBC_FLOOR}\"",
                "cargo build --locked --release --target \"$target\"",
            ),
            mutate(
                &workflow,
                "LANG=C readelf -W --version-info --dyn-syms \"$binary\"",
                "printf '2.28\\n'",
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
                FRANKENSEARCH_COMMIT,
                "3bbfd8c664062f8304e7a790c51794671f9214dc",
            ),
            mutate(
                &workflow,
                FAST_CMAES_COMMIT,
                "0000000000000000000000000000000000000000",
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
            mutate(&workflow, "\"$COSIGN_BIN\" verify-blob \\", "true \\"),
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
                "gh api \"/repos/${EXPECTED_REPOSITORY}/commits/${RELEASE_TAG}\" --jq '.sha'",
                "gh api \"/repos/${EXPECTED_REPOSITORY}/commits/main\" --jq '.sha'",
            ),
            mutate(&workflow, "case \"$release_count\" in", "case 0 in"),
            mutate(
                &workflow,
                "target_commitish: $revision",
                "target_commitish: \"main\"",
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
                "https://uploads.github.com/repos/${EXPECTED_REPOSITORY}/releases/${EXPECTED_RELEASE_ID}/assets?name=${asset_name}",
                "https://uploads.github.com/repos/${EXPECTED_REPOSITORY}/releases/${RELEASE_TAG}/assets?name=${asset_name}",
            ),
            mutate(&workflow, "--request POST \\", "--request PUT \\"),
            mutate(
                &workflow,
                "[ \"$matching_count\" -eq 1 ]",
                "[ \"$matching_count\" -ge 1 ]",
            ),
            mutate(
                &workflow,
                "--connect-timeout 30 --max-time 1800 \\",
                "--connect-timeout 30 \\",
            ),
            mutate(
                &workflow,
                "'(.id | type == \"number\") and .name == $name and .size == $size and .digest == $digest and .state == \"uploaded\"'",
                "'(.id | type == \"number\") and .name == $name and .size == $size'",
            ),
            mutate(
                &workflow,
                "validate_remote_subset \"$staged_assets\" false",
                "validate_remote_subset \"$staged_assets\" true",
            ),
            mutate(
                &workflow,
                "local_hash=\"${local_hashes[$asset_name]}\"",
                "local_hash=\"$(sha256sum \"publish/$asset_name\" | awk '{print $1}')\"",
            ),
            mutate(
                &workflow,
                "[ \"$uploaded_hash\" != \"$local_hash\" ]",
                "[ -z \"$uploaded_hash\" ]",
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
                "if ! cmp --silent \"$tag_installer\" publish/install.sh; then",
                "if [ ! -s publish/install.sh ]; then",
            ),
            mutate(
                &workflow,
                "output=\"$(timeout 30 git ls-remote --refs \\",
                "output=\"$(git ls-remote --refs \\",
            ),
            mutate(
                &workflow,
                "mcp_agent_mail_rust/${mcp_main_revision}/install.sh?cache_bust=${mcp_main_revision}",
                "mcp_agent_mail_rust/main/install.sh",
            ),
            mutate(
                &workflow,
                "if ! cmp --silent \"$tag_installer\" \"$main_installer\"; then",
                "if [ ! -s \"$main_installer\" ]; then",
            ),
            mutate(
                &workflow,
                "agentic_coding_flywheel_setup/${acfs_revision}/checksums.yaml?cache_bust=${acfs_revision}",
                "agentic_coding_flywheel_setup/main/checksums.yaml",
            ),
            mutate(
                &workflow,
                "marker = \"  mcp_agent_mail:\"",
                "marker = \"  mcp_agent_mail_old:\"",
            ),
            mutate(
                &workflow,
                "url: \"https://raw.githubusercontent.com/Dicklesworthstone/mcp_agent_mail_rust/refs/heads/main/install.sh\"",
                "url: \"https://example.invalid/install.sh\"",
            ),
            mutate(
                &workflow,
                "sha256: \"{expected_hash}\"",
                "sha256: \"{'0' * 64}\"",
            ),
            mutate(
                &workflow,
                "[ \"$acfs_revision_after\" != \"$acfs_revision\" ]",
                "[ -z \"$acfs_revision_after\" ]",
            ),
            mutate(
                &workflow,
                "[ \"$mcp_main_revision_after\" != \"$mcp_main_revision\" ]",
                "[ -z \"$mcp_main_revision_after\" ]",
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
        for (index, mutation) in mutations.into_iter().enumerate() {
            assert!(
                validate(&mutation).is_err(),
                "dist workflow contract mutation {index} unexpectedly passed"
            );
        }
    }
}

mod notify_acfs_contract {
    use std::fs;
    use std::path::{Path, PathBuf};

    const CHECKOUT_ACTION: &str = "actions/checkout@11bd71901bbe5b1630ceea73d27597364c9af683";

    fn workspace_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("conformance crate should have a workspace root")
            .to_path_buf()
    }

    fn read_workflow() -> String {
        let path = workspace_root().join(".github/workflows/notify-acfs.yml");
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

    fn validate(workflow: &str) -> Result<(), String> {
        for forbidden in [
            "pull_request:",
            "schedule:",
            "tags:",
            "types: [published]",
            "workflow_run:",
            "repository_dispatch:",
            "continue-on-error",
            "success()",
            "exit 0",
            "|| true",
            "--insecure",
            "ACFS_TOKEN: ${{ github.token }}",
            "ACFS_TOKEN: ${{ secrets.GITHUB_TOKEN }}",
        ] {
            if workflow.contains(forbidden) {
                return Err(format!(
                    "forbidden ACFS notification fallback remains: {forbidden}"
                ));
            }
        }

        let uses = workflow
            .lines()
            .filter_map(|line| line.trim().strip_prefix("uses: "))
            .collect::<Vec<_>>();
        if uses != ["actions/checkout@11bd71901bbe5b1630ceea73d27597364c9af683 # v4.2.2"] {
            return Err(format!("unexpected ACFS workflow action census: {uses:?}"));
        }
        require_once(workflow, CHECKOUT_ACTION)?;

        for needle in [
            "branches: [main]",
            "- 'install.sh'",
            "- 'Cargo.toml'",
            "workflow_dispatch:",
            "TOOL_NAME: \"mcp_agent_mail\"",
            "ACFS_REPOSITORY: \"Dicklesworthstone/agentic_coding_flywheel_setup\"",
            "EXPECTED_SOURCE_REPOSITORY: \"Dicklesworthstone/mcp_agent_mail_rust\"",
            "group: notify-acfs-${{ github.ref }}",
            "cancel-in-progress: false",
            "permissions:\n      contents: read",
            "persist-credentials: false",
            "if [[ ! -f install.sh || -L install.sh ]]; then",
            "installer_sha256=\"$(sha256sum install.sh | awk '{print $1}')\"",
            "if ! [[ \"$installer_sha256\" =~ ^[0-9a-f]{64}$ ]]; then",
            "echo \"sha256=$installer_sha256\" >> \"$GITHUB_OUTPUT\"",
            "ACFS_TOKEN: ${{ secrets.ACFS_REPO_DISPATCH_TOKEN }}",
            "INSTALLER_SHA256: ${{ steps.installer.outputs.sha256 }}",
            "SOURCE_REPOSITORY: ${{ github.repository }}",
            "SOURCE_REVISION: ${{ github.sha }}",
            "SOURCE_REF: ${{ github.ref }}",
            "if [[ -z \"${ACFS_TOKEN:-}\" ]]; then",
            "ACFS_REPO_DISPATCH_TOKEN is required; refusing to defer checksum authority to a poller",
            "if [[ \"$SOURCE_REF\" != refs/heads/main ]]; then",
            "if [[ \"$SOURCE_REPOSITORY\" != \"$EXPECTED_SOURCE_REPOSITORY\" ]]; then",
            "Refusing ACFS notification from unexpected repository ${SOURCE_REPOSITORY}",
            "--arg new_sha256 \"$INSTALLER_SHA256\" \\",
            "new_sha256: $new_sha256",
            "curl --fail-with-body --silent --show-error \\",
            "--connect-timeout 10 --max-time 30 \\",
            "--request POST \\",
            "--header \"Authorization: Bearer ${ACFS_TOKEN}\" \\",
            "--header \"Content-Type: application/json\" \\",
            "https://api.github.com/repos/${ACFS_REPOSITORY}/dispatches",
            "Publication remains blocked until ACFS",
        ] {
            require_once(workflow, needle)?;
        }
        require_exactly(workflow, "set -euo pipefail", 3)?;
        require_exactly(workflow, "exit 1", 5)?;
        require_in_order(
            workflow,
            &[
                "- name: Checkout the exact source revision",
                "- name: Bind notification to the exact installer bytes",
                "installer_sha256=\"$(sha256sum install.sh | awk '{print $1}')\"",
                "echo \"sha256=$installer_sha256\" >> \"$GITHUB_OUTPUT\"",
                "- name: Dispatch the exact checksum to ACFS",
                "if [[ -z \"${ACFS_TOKEN:-}\" ]]; then",
                "if [[ \"$SOURCE_REF\" != refs/heads/main ]]; then",
                "if [[ \"$SOURCE_REPOSITORY\" != \"$EXPECTED_SOURCE_REPOSITORY\" ]]; then",
                "--arg new_sha256 \"$INSTALLER_SHA256\" \\",
                "https://api.github.com/repos/${ACFS_REPOSITORY}/dispatches",
                "- name: Record the release-blocking checksum authority",
            ],
        )?;

        Ok(())
    }

    fn mutate(workflow: &str, from: &str, to: &str) -> String {
        let mutation = workflow.replacen(from, to, 1);
        assert_ne!(mutation, workflow, "mutation source was absent: {from}");
        mutation
    }

    #[test]
    fn notification_is_main_only_digest_bound_and_fail_closed() {
        let workflow = read_workflow();
        validate(&workflow).unwrap_or_else(|error| panic!("{error}"));
    }

    #[test]
    fn notification_contract_rejects_causal_bypasses() {
        let workflow = read_workflow();
        let mutations = [
            mutate(&workflow, CHECKOUT_ACTION, "actions/checkout@v4"),
            mutate(&workflow, "branches: [main]", "branches: [release]"),
            mutate(&workflow, "- 'install.sh'", "- 'README.md'"),
            mutate(&workflow, "- 'Cargo.toml'", "- 'Cargo.lock'"),
            mutate(
                &workflow,
                "if [[ -z \"${ACFS_TOKEN:-}\" ]]; then\n            echo \"::error::ACFS_REPO_DISPATCH_TOKEN is required; refusing to defer checksum authority to a poller\"\n            exit 1",
                "if [[ -z \"${ACFS_TOKEN:-}\" ]]; then\n            echo \"::warning::ACFS_REPO_DISPATCH_TOKEN is unavailable\"",
            ),
            mutate(
                &workflow,
                "INSTALLER_SHA256: ${{ steps.installer.outputs.sha256 }}",
                "INSTALLER_SHA256: ${{ github.sha }}",
            ),
            mutate(
                &workflow,
                "--arg new_sha256 \"$INSTALLER_SHA256\" \\",
                "--arg new_sha256 \"$SOURCE_REVISION\" \\",
            ),
            mutate(
                &workflow,
                "if [[ \"$SOURCE_REF\" != refs/heads/main ]]; then",
                "if [[ -z \"$SOURCE_REF\" ]]; then",
            ),
            mutate(
                &workflow,
                "if [[ \"$SOURCE_REPOSITORY\" != \"$EXPECTED_SOURCE_REPOSITORY\" ]]; then",
                "if [[ -z \"$SOURCE_REPOSITORY\" ]]; then",
            ),
            mutate(
                &workflow,
                "--connect-timeout 10 --max-time 30 \\",
                "--connect-timeout 10 \\",
            ),
            mutate(&workflow, "--request POST \\", "--request GET \\"),
        ];
        for mutation in mutations {
            assert!(
                validate(&mutation).is_err(),
                "ACFS notification contract mutation unexpectedly passed"
            );
        }
    }
}
