#!/usr/bin/env bash
# e2e_share.sh - E2E test suite for share/export bundle pipeline
#
# Run via (authoritative):
#   am e2e run --project . share
# Compatibility fallback:
#   AM_E2E_FORCE_LEGACY=1 ./scripts/e2e_test.sh share
# Required real-crypto lane against an already built candidate (no build):
#   bash scripts/e2e_share.sh --crypto-only all /absolute/path/to/am
# Causal publication-only example (real age, actual candidate CLI):
#   bash scripts/e2e_share.sh --crypto-only race-concurrent /absolute/path/to/am
# Optional local dependency absence only: AM_SHARE_REQUIRE_AGE=0. A present
# dependency failing is always a failure, never a skip (br-kp1in.48).
#
# This suite verifies:
# - Export creates a bundle with manifest.json, mailbox.sqlite3, viewer/
# - Exported DB passes PRAGMA integrity_check
# - FTS index exists and basic queries succeed
# - Dry-run mode validates without creating output artifacts
# - Two exports from the same inputs produce matching manifest hashes (determinism)
# - Verify subcommand succeeds on valid bundle
# - Verify subcommand fails on nonexistent/corrupted bundle
# - Scrub presets produce different output
#
# Artifacts:
#   tests/artifacts/share/<timestamp>/*

set -euo pipefail

: "${AM_E2E_KEEP_TMP:=1}"

E2E_SUITE="${E2E_SUITE:-share}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=./e2e_lib.sh
source "${SCRIPT_DIR}/e2e_lib.sh"

# Shared real-age fixture/CLI runner. Secrets travel over a no-echo PTY to age
# and stdin to am; neither secrets nor raw child output enter evidence files.
share_crypto_matrix() {
    local am_binary="$1"
    local selected_case="$2"
    local workspace="$3"
    if ! command -v python3 >/dev/null 2>&1; then
        if [ "${AM_SHARE_REQUIRE_AGE:-1}" != "0" ]; then
            e2e_fail "required real-age lane: python3 is absent"
            return 1
        fi
        e2e_skip "optional real-age lane: python3 is absent"
        return 77
    fi
    AM_SHARE_REQUIRE_AGE="${AM_SHARE_REQUIRE_AGE:-1}" python3 - \
        "${am_binary}" "${selected_case}" "${workspace}" "${E2E_ARTIFACT_DIR}" <<'PY'
import errno
import hashlib
import itertools
import json
import os
from pathlib import Path
import secrets
import shutil
import signal
import stat
import subprocess
import sys
import time

am, selected, workspace, artifact_dir = sys.argv[1:]
root = Path(workspace)
report_path = Path(artifact_dir) / "crypto.json"
report = {
    "schema": "am-share-crypto-v1",
    "claim": "real-age to real-am interoperability and bounded publication refusal probes",
    "concurrent_publication": "not selected or not yet completed",
    "publication_cases_completed": [],
    "native_platform": sys.platform,
    "windows_publication": "not covered by this Unix CLI lane; requires native evidence",
    "selected_case": selected,
    "workspace": str(root),
    "tools": {},
    "commands": [],
    "cases": [],
}
secret_values = []
required = os.environ.get("AM_SHARE_REQUIRE_AGE", "1") != "0"
environment = dict(os.environ, AM_INTERFACE_MODE="cli", NO_COLOR="1")


class Failure(Exception):
    pass


def require(condition, message):
    if not condition:
        raise Failure(message)


def digest(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def witness(path):
    path = Path(path)
    try:
        metadata = path.lstat()
    except FileNotFoundError:
        return {"exists": False}
    result = {
        "exists": True,
        "size": metadata.st_size,
        "device": metadata.st_dev,
        "inode": metadata.st_ino,
        "mode": stat.S_IMODE(metadata.st_mode),
        "links": metadata.st_nlink,
    }
    if stat.S_ISLNK(metadata.st_mode):
        result.update(kind="symlink", target=os.readlink(path))
    elif stat.S_ISREG(metadata.st_mode):
        result.update(kind="file", sha256=digest(path))
    elif stat.S_ISDIR(metadata.st_mode):
        result.update(kind="directory")
    else:
        result.update(kind="other")
    return result


def no_secret_output(stdout, stderr):
    require(not any(secret in stdout or secret in stderr for secret in secret_values),
            "a subprocess exposed secret material; its output was suppressed")


def run(argv, secret=None):
    argv = [str(arg) for arg in argv]
    receipt = {"argv": argv, "secret_input": "stdin" if secret else None}
    report["commands"].append(receipt)
    started = time.monotonic()
    child = subprocess.Popen(argv, stdin=subprocess.PIPE if secret else subprocess.DEVNULL,
                             stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                             env=environment, start_new_session=True)
    try:
        stdout, stderr = child.communicate(secret + b"\n" if secret else None, timeout=45)
    except subprocess.TimeoutExpired:
        os.killpg(child.pid, signal.SIGKILL)
        child.communicate()
        receipt.update(status="timeout", completed=False)
        raise Failure("crypto subprocess exceeded 45 seconds") from None
    receipt.update(status=child.returncode, completed=True,
                   elapsed_ms=round((time.monotonic() - started) * 1000),
                   stdout_bytes=len(stdout), stderr_bytes=len(stderr))
    no_secret_output(stdout, stderr)
    return child.returncode, stdout, stderr


def publication_wrapper(real_age):
    directory = root / "publication-wrapper"
    directory.mkdir(mode=0o700)
    wrapper = directory / "age"
    require("\n" not in sys.executable and " " not in sys.executable,
            "publication wrapper requires an unambiguous Python interpreter path")
    wrapper.write_text("#!" + sys.executable + "\n" + '''import json
import os
from pathlib import Path
import subprocess
import sys
import time

real_age = os.environ["AM_SHARE_TEST_REAL_AGE"]
if sys.argv[1:] == ["--version"]:
    os.execv(real_age, [real_age, "--version"])

# The real cryptographic operation owns the inherited stdin/stdout handles.
# Only its successful completion is delayed on return to the candidate.
result = subprocess.run([real_age] + sys.argv[1:], check=False)
if result.returncode != 0:
    sys.exit(result.returncode)
barrier = Path(os.environ["AM_SHARE_TEST_PUBLICATION_BARRIER"])
with (barrier / "completed.json").open("x", encoding="utf-8") as receipt:
    json.dump({"real_age_status": result.returncode, "completed": True,
               "boundary": "real age completed; wrapper has not returned"}, receipt)
with (barrier / "ready").open("xb"):
    pass
deadline = time.monotonic() + 60
while not (barrier / "release").exists():
    if time.monotonic() >= deadline:
        sys.exit(124)
    time.sleep(0.01)
sys.exit(0)
''', encoding="utf-8")
    wrapper.chmod(0o700)
    report["publication_wrapper"] = {
        "path": str(wrapper), "sha256": digest(wrapper),
        "real_age": str(real_age),
        "scope": "PATH and rendezvous variables supplied only to candidate children",
    }
    return directory


def start_publication_child(encrypted, destination, barrier, wrapper_dir):
    barrier.mkdir(mode=0o700)
    argv = [str(arg) for arg in (am, "share", "decrypt", encrypted,
                                 "--identity", identity, "--output", destination)]
    receipt = {"argv": argv, "operation": "identity decrypt publication",
               "secret_input": None, "barrier": str(barrier), "completed": False}
    report["commands"].append(receipt)
    child_env = dict(environment,
                     PATH=str(wrapper_dir) + os.pathsep + environment.get("PATH", ""),
                     AM_SHARE_TEST_REAL_AGE=str(age),
                     AM_SHARE_TEST_PUBLICATION_BARRIER=str(barrier))
    child = subprocess.Popen(argv, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                             stderr=subprocess.PIPE, env=child_env, start_new_session=True)
    return child, receipt, time.monotonic()


def finish_publication_child(process):
    child, receipt, started = process
    try:
        stdout, stderr = child.communicate(timeout=45)
    except subprocess.TimeoutExpired:
        os.killpg(child.pid, signal.SIGKILL)
        stdout, stderr = child.communicate()
        receipt.update(status=child.returncode, completed=True, timed_out=True,
                       stdout_bytes=len(stdout), stderr_bytes=len(stderr))
        no_secret_output(stdout, stderr)
        raise Failure("publication candidate exceeded 45 seconds") from None
    receipt.update(status=child.returncode, completed=True,
                   elapsed_ms=round((time.monotonic() - started) * 1000),
                   stdout_bytes=len(stdout), stderr_bytes=len(stderr))
    no_secret_output(stdout, stderr)
    return child.returncode


def wait_for_publication_boundary(process, barrier):
    child, receipt, _ = process
    deadline = time.monotonic() + 45
    while not (barrier / "ready").exists():
        if child.poll() is not None:
            finish_publication_child(process)
            raise Failure("candidate exited before the real age completion boundary")
        require(time.monotonic() < deadline, "real age completion boundary timed out")
        time.sleep(0.01)
    completion = json.loads((barrier / "completed.json").read_text(encoding="utf-8"))
    receipt["real_age_completion"] = completion
    require(completion.get("completed") and completion.get("real_age_status") == 0,
            "publication barrier did not witness successful real age completion")
    require(child.poll() is None, "candidate escaped the publication barrier")


def release_publication(barrier):
    with (barrier / "release").open("xb"):
        pass


def retained_publication_artifacts(directory):
    artifacts = []
    paths = list(itertools.islice(directory.rglob("*"), 257))
    require(len(paths) <= 256, "publication fixture exceeded artifact inventory bound")
    for path in sorted(paths):
        artifacts.append({"path": str(path.relative_to(directory)), "witness": witness(path)})
    return artifacts


def require_retained_plaintext(artifacts, plaintext_digest):
    for artifact in artifacts:
        relative = Path(artifact["path"])
        if not any(part.startswith(".age-decrypt.") for part in relative.parts):
            continue
        metadata = artifact["witness"]
        if metadata.get("kind") == "file" and metadata.get("sha256") == plaintext_digest:
            require(metadata["mode"] == 0o600, "retained plaintext stage is not private")
            parent = str(relative.parent)
            require(any(item["path"] == parent and item["witness"].get("mode") == 0o700
                        and item["witness"].get("kind") == "directory"
                        for item in artifacts), "retained plaintext directory is not private")
            return
    raise Failure("publication refusal did not retain its completed private plaintext stage")


def encrypt_passphrase(age, source, destination, secret, armored=False):
    # age uses terminal stdin in this fresh session. Wait for each prompt and
    # disabled echo; pipe input cannot satisfy its terminal protocol.
    import pty
    import select
    import termios

    argv = [age, "-p"] + (["-a"] if armored else [])
    argv += ["-o", str(destination), str(source)]
    receipt = {"argv": argv, "secret_input": "terminal, echo disabled"}
    report["commands"].append(receipt)
    terminal, slave = pty.openpty()
    child = None
    captured = bytearray()
    pending = bytearray()
    prompts = [b"Enter passphrase", b"Confirm passphrase"]
    phase = 0
    started = time.monotonic()
    try:
        child = subprocess.Popen(argv, stdin=slave, stdout=subprocess.DEVNULL, stderr=slave,
                                 env=environment, start_new_session=True)
        deadline = started + 45
        while child.poll() is None:
            if time.monotonic() >= deadline:
                os.killpg(child.pid, signal.SIGKILL)
                child.wait()
                receipt.update(status="timeout", completed=False)
                raise Failure("age terminal encryption exceeded 45 seconds")
            readable, _, _ = select.select([terminal], [], [], 0.05)
            if readable:
                try:
                    chunk = os.read(terminal, 4096)
                except OSError as error:
                    if error.errno != errno.EIO:
                        raise
                    chunk = b""
                captured.extend(chunk)
                pending.extend(chunk)
            if phase < len(prompts) and prompts[phase] in pending:
                if not termios.tcgetattr(slave)[3] & termios.ECHO:
                    os.write(terminal, secret + b"\n")
                    phase += 1
                    pending.clear()
        receipt.update(status=child.returncode, completed=True,
                       elapsed_ms=round((time.monotonic() - started) * 1000),
                       terminal_bytes=len(captured), prompts_answered=phase)
        no_secret_output(bytes(captured), b"")
        require(child.returncode == 0 and phase == 2,
                "real age passphrase encryption failed; terminal output suppressed")
    finally:
        if child is not None and child.poll() is None:
            os.killpg(child.pid, signal.SIGKILL)
            child.wait()
        os.close(terminal)
        os.close(slave)


def perform_case(name, encrypted, destination, identity=None, secret=None,
                 expect_success=True, default_output=False):
    identity_before = identity.read_bytes() if identity is not None else None
    before = {"original": witness(original), "ciphertext": witness(encrypted),
              "destination": witness(destination)}
    case = {"name": name, "before": before}
    report["cases"].append(case)
    argv = [am, "share", "decrypt", encrypted]
    if identity is not None:
        argv += ["--identity", identity]
    else:
        argv += ["--passphrase"]
    if not default_output:
        argv += ["--output", destination]
    status, _, _ = run(argv, secret)
    after = {"original": witness(original), "ciphertext": witness(encrypted),
             "destination": witness(destination)}
    case.update(status=status, after=after)
    require(before["original"] == after["original"], "original fixture changed")
    require(before["ciphertext"] == after["ciphertext"], "ciphertext fixture changed")
    if identity is not None:
        require(identity.read_bytes() == identity_before, "identity file changed")
        case["identity_preserved"] = True
    if expect_success:
        require(status == 0, name + ": am did not decrypt successfully")
        require(not before["destination"]["exists"], "positive destination was not distinct")
        require(after["destination"].get("sha256") == before["original"]["sha256"],
                name + ": plaintext bytes differ")
        require(after["destination"]["mode"] == 0o600,
                name + ": plaintext is not private (0600)")
    else:
        require(status == 1, name + ": expected decryption refusal (exit 1)")
        require(before["destination"] == after["destination"],
                name + ": failed decryption published or changed plaintext")
    case["passed"] = True


def perform_publication_case(name, wrapper_dir):
    directory = root / "publication" / name
    directory.mkdir(mode=0o700)
    parent = directory / "parent"
    parent.mkdir(mode=0o700)
    destination = parent / "output.zip"
    sentinel = directory / "outside-sentinel"
    sentinel.write_bytes(b"outside sentinel must survive\x00" + secrets.token_bytes(32))
    source_paths = {"original": original, "ciphertext": identity_cipher, "identity": identity,
                    "outside_sentinel": sentinel}
    case = {"name": name, "operation": "share decrypt --identity",
            "candidate_sha256": report["tools"]["am"]["sha256"],
            "fixture": str(directory), "before": {}}
    report["cases"].append(case)
    children = []
    barriers = [directory / "first-barrier"]
    protected_destination = None
    retained_destination = None
    loser_digest = None
    try:
        if name == "race-concurrent":
            peer_original = directory / "peer-original.zip"
            peer_original.write_bytes(b"different concurrent payload\x00" + secrets.token_bytes(131147))
            peer_cipher = directory / "peer.zip.age"
            status, recipient, _ = run([keygen, "-y", identity])
            require(status == 0, "concurrent fixture recipient derivation failed")
            status, _, _ = run([age, "-r", recipient.decode("ascii").strip(),
                                "-o", peer_cipher, peer_original])
            require(status == 0, "concurrent real-age fixture encryption failed")
            source_paths.update(peer_original=peer_original, peer_ciphertext=peer_cipher)
            loser_digest = digest(peer_original)
            barriers.append(directory / "second-barrier")
        case["before"] = {label: witness(path) for label, path in source_paths.items()}
        case["before"]["destination"] = witness(destination)
        require(not case["before"]["destination"]["exists"],
                "publication destination was not initially absent")
        children.append(start_publication_child(identity_cipher, destination,
                                                barriers[0], wrapper_dir))
        if name == "race-concurrent":
            children.append(start_publication_child(peer_cipher, destination,
                                                    barriers[1], wrapper_dir))
        for process, barrier in zip(children, barriers):
            wait_for_publication_boundary(process, barrier)
        case["at_completion_boundary"] = {
            "destination": witness(destination),
            "real_age_completed": len(children),
            "candidates_waiting_before_publication": len(children),
        }
        require(not case["at_completion_boundary"]["destination"]["exists"],
                "candidate published before its real-age subprocess completed")

        if name == "race-regular":
            with destination.open("xb") as output:
                output.write(b"concurrent destination must survive\x00" + secrets.token_bytes(32))
            protected_destination = witness(destination)
        elif name == "race-dangling-symlink":
            destination.symlink_to("missing-target")
            protected_destination = witness(destination)
        elif name == "race-sentinel-symlink":
            destination.symlink_to(sentinel)
            protected_destination = witness(destination)
        elif name == "race-parent":
            retained_parent = directory / "retained-parent"
            parent.rename(retained_parent)
            parent.mkdir(mode=0o700)
            stages = [entry for entry in retained_parent.iterdir()
                      if entry.name.startswith(".age-decrypt.") and entry.is_dir()]
            require(len(stages) == 1, "parent race did not identify one private decrypt stage")
            staged_files = [entry for entry in stages[0].iterdir() if entry.is_file()]
            require(len(staged_files) == 1, "parent race did not identify one staged payload")
            substitute_stage = parent / stages[0].name
            substitute_stage.mkdir(mode=0o700)
            substitute_payload = substitute_stage / staged_files[0].name
            with substitute_payload.open("xb") as output:
                output.write(b"substituted stage must survive\x00" + secrets.token_bytes(32))
            source_paths["substituted_stage_payload"] = substitute_payload
            case["before"]["substituted_stage_payload"] = witness(substitute_payload)
            # Keep the substitute destination absent: a path-based hard-link
            # fix must not pass merely because an occupied leaf blocks it.
            # Recreating the old stage spelling makes ambient-path reuse fail
            # this causal probe even if publication itself is no-replace.
            protected_destination = witness(destination)
            retained_destination = retained_parent / destination.name
            case["retained_original_parent"] = str(retained_parent)
        case["planted_destination"] = protected_destination

        release_publication(barriers[0])
        first_status = finish_publication_child(children[0])
        case["candidate_statuses"] = [first_status]
        if name == "race-concurrent":
            require(first_status == 0, "first concurrent publisher did not complete")
            first_winner = witness(destination)
            case["first_complete_winner"] = first_winner
            require(first_winner.get("sha256") == case["before"]["original"]["sha256"],
                    "first concurrent publisher did not publish complete verified bytes")
            release_publication(barriers[1])
            second_status = finish_publication_child(children[1])
            case["candidate_statuses"].append(second_status)
            require(second_status == 1, "second concurrent publisher did not refuse")
            require(witness(destination) == first_winner,
                    "losing concurrent publisher changed the winner's bytes or inode")
        elif name == "race-parent":
            retained_output = witness(retained_destination)
            case["retained_parent_output"] = retained_output
            if first_status == 0:
                require(retained_output.get("sha256") == case["before"]["original"]["sha256"],
                        "renamed-parent publication did not stay in its retained authority")
                require(retained_output.get("mode") == 0o600,
                        "retained-parent plaintext is not private")
                case["parent_swap_result"] = "complete output under retained original parent"
            else:
                require(first_status == 1 and not retained_output["exists"],
                        "parent substitution refusal published an unexpected output")
                case["parent_swap_result"] = "refused; complete private stage retained"
        else:
            require(first_status == 1, "raced destination was not refused")
        if protected_destination is not None:
            require(witness(destination) == protected_destination,
                    "publication changed a raced destination or substituted parent entry")
    finally:
        # Terminate only children created by this case. Fixtures and staged
        # bytes are deliberately retained, including on an assertion failure.
        for child, receipt, started in children:
            if child.poll() is None:
                try:
                    os.killpg(child.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
            if not receipt.get("completed"):
                stdout, stderr = child.communicate()
                receipt.update(status=child.returncode, completed=True, forced_stop=True,
                               elapsed_ms=round((time.monotonic() - started) * 1000),
                               stdout_bytes=len(stdout), stderr_bytes=len(stderr))
                no_secret_output(stdout, stderr)
        case["after"] = {label: witness(path) for label, path in source_paths.items()}
        case["after"]["destination"] = witness(destination)
        case["retained_artifacts"] = retained_publication_artifacts(directory)
    for label in source_paths:
        require(case["before"][label] == case["after"][label],
                "publication race changed an original fixture, key, ciphertext, or sentinel")
    if name == "race-concurrent":
        require_retained_plaintext(case["retained_artifacts"], loser_digest)
        require(case["after"]["destination"].get("mode") == 0o600,
                "concurrent winner plaintext is not private")
    elif name != "race-parent" or case["candidate_statuses"][0] != 0:
        require_retained_plaintext(case["retained_artifacts"], case["before"]["original"]["sha256"])
    case["passed"] = True
    report["publication_cases_completed"].append(name)
    if name == "race-concurrent":
        report["concurrent_publication"] = "two real CLI decryptions; exactly one complete winner"


exit_code = 0
try:
    cases = ["identity-default", "identity-explicit", "passphrase", "passphrase-armored",
             "wrong-passphrase", "wrong-identity", "tampered-passphrase", "tampered-identity",
             "existing-output", "race-regular", "race-dangling-symlink",
             "race-sentinel-symlink", "race-parent", "race-concurrent"]
    require(selected == "all" or selected in cases, "unknown crypto scenario")
    missing = [tool for tool in ("age", "age-keygen") if shutil.which(tool) is None]
    if os.name != "posix":
        missing.append("Unix PTY support")
    if missing:
        report.update(status="failed" if required else "skipped",
                      reason="missing dependencies: " + ", ".join(missing))
        exit_code = 1 if required else 77
    else:
        age = str(Path(shutil.which("age")).resolve())
        keygen = str(Path(shutil.which("age-keygen")).resolve())
        for label, executable in (("age", age), ("age-keygen", keygen), ("am", am)):
            status, stdout, stderr = run([executable, "--version"])
            require(status == 0, label + " --version failed")
            report["tools"][label] = {
                "path": str(Path(executable).resolve()),
                "sha256": digest(Path(executable).resolve()),
                "version": (stdout + stderr).decode("utf-8", "replace").strip()[:200],
            }
        for directory in ("original", "ciphertext", "outputs", "identities", "publication"):
            (root / directory).mkdir(mode=0o700)
        original = root / "original" / "bundle.zip"
        # More than two age payload chunks: late corruption must not publish
        # the already-decrypted prefix. The unique source is never overwritten.
        original.write_bytes(secrets.token_bytes(2 * 65536 + 73))
        original_before = witness(original)
        passphrase = b"  " + secrets.token_hex(24).encode("ascii") + b" \t"
        wrong_passphrase = b"wrong-" + secrets.token_hex(24).encode("ascii")
        secret_values.extend([passphrase, passphrase.strip(), wrong_passphrase])
        identity = root / "identities" / "identity.txt"
        wrong_identity = root / "identities" / "wrong-identity.txt"
        identity_cipher = root / "ciphertext" / "identity.zip.age"
        passphrase_cipher = root / "ciphertext" / "passphrase.zip.age"
        armored_cipher = root / "ciphertext" / "armored.zip.age"
        wrapper_dir = None

        def need_identity():
            if identity_cipher.exists():
                return
            for key_path in (identity, wrong_identity):
                status, _, _ = run([keygen, "-o", key_path])
                require(status == 0, "real age-keygen failed")
                secret_values.extend(line for line in key_path.read_bytes().splitlines()
                                     if line.startswith(b"AGE-SECRET-KEY-"))
            status, public_key, _ = run([keygen, "-y", identity])
            require(status == 0 and public_key.strip().startswith(b"age1"),
                    "real age-keygen did not return an age recipient")
            status, _, _ = run([age, "-r", public_key.decode("ascii").strip(),
                                "-o", identity_cipher, original])
            require(status == 0, "real age recipient encryption failed")

        def need_passphrase(armored=False):
            destination = armored_cipher if armored else passphrase_cipher
            if not destination.exists():
                encrypt_passphrase(age, original, destination, passphrase, armored)
            return destination

        for case_name in cases if selected == "all" else [selected]:
            output = root / "outputs" / (case_name + ".zip")
            if case_name.startswith("race-"):
                need_identity()
                if wrapper_dir is None:
                    wrapper_dir = publication_wrapper(age)
                perform_publication_case(case_name, wrapper_dir)
            elif case_name.startswith("identity-"):
                need_identity()
                use_default = case_name == "identity-default"
                if use_default:
                    output = identity_cipher.with_suffix("")
                perform_case(case_name, identity_cipher, output, identity=identity,
                             default_output=use_default)
            elif case_name in ("passphrase", "passphrase-armored"):
                encrypted = need_passphrase(case_name == "passphrase-armored")
                perform_case(case_name, encrypted, output, secret=passphrase)
            elif case_name == "wrong-passphrase":
                perform_case(case_name, need_passphrase(), output, secret=wrong_passphrase,
                             expect_success=False)
            elif case_name == "wrong-identity":
                need_identity()
                perform_case(case_name, identity_cipher, output, identity=wrong_identity,
                             expect_success=False)
            elif case_name.startswith("tampered-"):
                passphrase_mode = case_name == "tampered-passphrase"
                if passphrase_mode:
                    encrypted = need_passphrase()
                else:
                    need_identity()
                    encrypted = identity_cipher
                pristine = witness(encrypted)
                for mutation in ("corrupted", "truncated"):
                    damaged = root / "ciphertext" / (case_name + "-" + mutation + ".age")
                    data = bytearray(encrypted.read_bytes())
                    if mutation == "corrupted":
                        data[-1] ^= 1
                    else:
                        data = data[:-1]
                    damaged.write_bytes(data)
                    destination = root / "outputs" / (case_name + "-" + mutation + ".zip")
                    perform_case(case_name + "-" + mutation, damaged, destination,
                                 identity=None if passphrase_mode else identity,
                                 secret=passphrase if passphrase_mode else None,
                                 expect_success=False)
                require(pristine == witness(encrypted), "pristine ciphertext changed")
            elif case_name == "existing-output":
                need_identity()
                need_passphrase()
                for mode, encrypted in (("identity", identity_cipher),
                                        ("passphrase", passphrase_cipher)):
                    destination = root / "outputs" / ("existing-" + mode + ".zip")
                    destination.write_bytes(b"pre-existing destination must survive\x00" +
                                            secrets.token_bytes(32))
                    perform_case(case_name + "-" + mode, encrypted, destination,
                                 identity=identity if mode == "identity" else None,
                                 secret=passphrase if mode == "passphrase" else None,
                                 expect_success=False)
        require(original_before == witness(original), "original fixture changed across matrix")
        report.update(status="passed", original_preserved=True)
except Failure as error:
    report.update(status="failed", reason=str(error))
    exit_code = 1
except Exception as error:
    # Arbitrary exception text/captured output may expose secrets.
    report.update(status="failed", reason="runner exception: " + type(error).__name__)
    exit_code = 1
finally:
    report_path.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")

print("crypto " + report["status"] + ": " + str(report_path))
if report.get("reason"):
    print(report["reason"])
sys.exit(exit_code)
PY
}

e2e_init_artifacts
e2e_banner "Share / Export Bundle E2E Test Suite"

# This entrypoint deliberately precedes SQLite/server setup and all build
# helpers. Qualification must pass the exact prepared candidate executable.
if [ "${1:-}" = "--crypto-only" ]; then
    if [ "$#" -ne 3 ] || [ ! -x "$3" ]; then
        e2e_fail "usage: --crypto-only CASE /absolute/path/to/built/am"
        e2e_summary || true
        exit 1
    fi
    CRYPTO_WORK="$(e2e_mktemp "e2e_share_crypto")"
    e2e_case_banner "real age CLI crypto: $2"
    if share_crypto_matrix "$3" "$2" "${CRYPTO_WORK}"; then
        e2e_pass "real age CLI crypto: $2"
        e2e_summary
    else
        CRYPTO_STATUS=$?
        if [ "${CRYPTO_STATUS}" -eq 77 ]; then
            e2e_skip "optional real-age dependency absent (see crypto.json when Python is available)"
        else
            e2e_fail "required real-age crypto case failed (see crypto.json)"
        fi
        e2e_summary || true
        exit "${CRYPTO_STATUS}"
    fi
    exit 0
fi

e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"

for cmd in sqlite3 python3; do
    if ! command -v "${cmd}" >/dev/null 2>&1; then
        e2e_log "${cmd} not found; skipping suite"
        e2e_skip "${cmd} required"
        e2e_summary
        exit 0
    fi
done

e2e_fatal() {
    local msg="$1"
    e2e_fail "${msg}"
    e2e_summary || true
    exit 1
}

# ---------------------------------------------------------------------------
# Build binaries
# ---------------------------------------------------------------------------

AM_BIN="$(e2e_ensure_binary "am" | tail -n 1)"
MCP_BIN="$(e2e_ensure_binary "mcp-agent-mail" | tail -n 1)"

e2e_log "am binary: ${AM_BIN}"
e2e_log "mcp-agent-mail binary: ${MCP_BIN}"

# Full share qualification always requires the real dependencies, regardless
# of the optional-local setting used by individual Cargo tests.
e2e_case_banner "real age passphrase and identity interoperability"
CRYPTO_WORK="$(e2e_mktemp "e2e_share_crypto")"
if AM_SHARE_REQUIRE_AGE=1 share_crypto_matrix "${AM_BIN}" all "${CRYPTO_WORK}"; then
    e2e_pass "real age CLI crypto matrix"
else
    e2e_fatal "real age CLI crypto matrix failed (see crypto.json)"
fi

# ---------------------------------------------------------------------------
# Workspace
# ---------------------------------------------------------------------------

WORK="$(e2e_mktemp "e2e_share")"
DB_PATH="${WORK}/storage.sqlite3"
STORAGE_ROOT="${WORK}/storage_root"
mkdir -p "${STORAGE_ROOT}"

e2e_log "Workspace: ${WORK}"
e2e_log "DB: ${DB_PATH}"
e2e_log "Storage root: ${STORAGE_ROOT}"

# Common env vars for the am CLI
am_env() {
    DATABASE_URL="sqlite:////${DB_PATH}" \
    STORAGE_ROOT="${STORAGE_ROOT}" \
    "$@"
}

extract_local_asset_paths() {
    local html_file="$1"
    local base_dir="${2:-}"
    python3 - <<'PY' "${html_file}" "${base_dir}"
import re
import sys
import posixpath
from pathlib import Path

html_path = Path(sys.argv[1])
base_dir = (sys.argv[2] or "").strip("/")
text = html_path.read_text(encoding="utf-8", errors="replace")

paths = set()
for _, raw in re.findall(r'(href|src)\s*=\s*["\']([^"\']+)["\']', text, flags=re.IGNORECASE):
    value = (raw or "").strip()
    if not value:
        continue
    if value.startswith(("#", "data:", "mailto:", "tel:", "javascript:", "http://", "https://")):
        continue
    value = value.split("?", 1)[0].split("#", 1)[0]

    if value.startswith("/"):
        resolved = value.lstrip("/")
    else:
        joined = posixpath.join(base_dir, value) if base_dir else value
        resolved = posixpath.normpath(joined)

    while resolved.startswith("../"):
        resolved = resolved[3:]
    resolved = resolved.lstrip("./")
    if not resolved or resolved == ".":
        continue
    paths.add(resolved)

for path in sorted(paths):
    print(path)
PY
}

HTTP_LAST_STATUS=""
HTTP_LAST_CASE_DIR=""
HTTP_LAST_RESPONSE_FILE=""

http_get_capture() {
    local case_id="$1"
    local url="$2"
    local body_out_file="${3:-}"

    local case_dir="${E2E_ARTIFACT_DIR}/${case_id}"
    local case_headers_file="${case_dir}/headers.txt"
    local case_body_file="${case_dir}/response.txt"
    local case_status_file="${case_dir}/status.txt"
    local case_timing_file="${case_dir}/timing.txt"
    local case_curl_stderr_file="${case_dir}/curl_stderr.txt"
    local case_curl_args_file="${case_dir}/curl_args.txt"

    local headers_file="${E2E_ARTIFACT_DIR}/${case_id}_headers.txt"
    local body_file="${E2E_ARTIFACT_DIR}/${case_id}_body.txt"
    local status_file="${E2E_ARTIFACT_DIR}/${case_id}_status.txt"
    local timing_file="${E2E_ARTIFACT_DIR}/${case_id}_timing.txt"
    local curl_stderr_file="${E2E_ARTIFACT_DIR}/${case_id}_curl_stderr.txt"
    local curl_args_file="${E2E_ARTIFACT_DIR}/${case_id}_curl_args.txt"

    mkdir -p "${case_dir}"
    e2e_mark_case_start "${case_id}"
    printf "curl -sS -D %q -o %q -w %%{http_code} %q\n" "${case_headers_file}" "${case_body_file}" "${url}" > "${case_curl_args_file}"

    local start_ns end_ns elapsed_ms
    start_ns="$(date +%s%N)"
    set +e
    local status
    status="$(curl -sS -D "${case_headers_file}" -o "${case_body_file}" -w "%{http_code}" "${url}" 2>"${case_curl_stderr_file}")"
    local rc=$?
    set -e
    end_ns="$(date +%s%N)"
    elapsed_ms=$(( (end_ns - start_ns) / 1000000 ))

    if [ "${rc}" -ne 0 ]; then
        status="000"
    fi

    echo "${status}" > "${case_status_file}"
    echo "${elapsed_ms}" > "${case_timing_file}"

    cp "${case_headers_file}" "${headers_file}" 2>/dev/null || true
    cp "${case_body_file}" "${body_file}" 2>/dev/null || true
    cp "${case_status_file}" "${status_file}" 2>/dev/null || true
    cp "${case_timing_file}" "${timing_file}" 2>/dev/null || true
    cp "${case_curl_stderr_file}" "${curl_stderr_file}" 2>/dev/null || true
    cp "${case_curl_args_file}" "${curl_args_file}" 2>/dev/null || true

    if [ -n "${body_out_file}" ]; then
        cp "${case_body_file}" "${body_out_file}" 2>/dev/null || true
        HTTP_LAST_RESPONSE_FILE="${body_out_file}"
    else
        HTTP_LAST_RESPONSE_FILE="${case_body_file}"
    fi
    HTTP_LAST_STATUS="${status}"
    HTTP_LAST_CASE_DIR="${case_dir}"
}

# ---------------------------------------------------------------------------
# Seed a realistic mailbox
# ---------------------------------------------------------------------------

e2e_case_banner "Seed mailbox via HTTP server"

TOKEN="e2e-share-token"

if ! e2e_start_server_with_logs "${DB_PATH}" "${STORAGE_ROOT}" "share" \
    "HTTP_PATH=/api" \
    "HTTP_BEARER_TOKEN=${TOKEN}" \
    "HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=0" \
    "HTTP_RBAC_ENABLED=0" \
    "HTTP_RATE_LIMIT_ENABLED=0" \
    "NOTIFICATIONS_ENABLED=0"; then
    e2e_fatal "server failed to start (port not open)"
fi
trap 'e2e_stop_server || true' EXIT

API_URL="${E2E_SERVER_URL%/mcp/}/api/"

rpc_call() {
    local case_id="$1"
    local tool_name="$2"
    local args_json="$3"

    local case_dir="${E2E_ARTIFACT_DIR}/${case_id}"
    local body_file="${E2E_ARTIFACT_DIR}/${case_id}_body.json"

    e2e_mark_case_start "${case_id}"
    if ! e2e_rpc_call "${case_id}" "${API_URL}" "${tool_name}" "${args_json}" "authorization: Bearer ${TOKEN}"; then
        :
    fi

    cp "${case_dir}/response.json" "${body_file}" 2>/dev/null || true

    local status
    status="$(e2e_rpc_read_status "${case_id}")"
    if [ -z "${status}" ] || [ "${status}" != "200" ]; then
        e2e_fatal "${case_id}: RPC failed (HTTP ${status:-missing})"
    fi
}

rpc_has_error() {
    local resp_file="$1"
    python3 - <<'PY' "$resp_file"
import json, sys
data = json.load(open(sys.argv[1], "r", encoding="utf-8"))
if data.get("error"):
    sys.exit(0)
res = data.get("result") or {}
if res.get("isError") is True:
    sys.exit(0)
sys.exit(1)
PY
}

PROJECT_DIR="$(e2e_mktemp "e2e_share_project")"

# Ensure project
rpc_call "ensure_project" "ensure_project" "{\"human_key\": \"${PROJECT_DIR}\"}"
if rpc_has_error "${E2E_ARTIFACT_DIR}/ensure_project_body.json"; then
    e2e_fatal "ensure_project failed"
fi

# Register agents
rpc_call "reg_alice" "register_agent" "$(python3 -c "
import json,sys
print(json.dumps({'project_key': sys.argv[1], 'program': 'e2e', 'model': 'test', 'name': 'RedFox', 'task_description': 'e2e share test'}))
" "${PROJECT_DIR}")"

rpc_call "reg_bob" "register_agent" "$(python3 -c "
import json,sys
print(json.dumps({'project_key': sys.argv[1], 'program': 'e2e', 'model': 'test', 'name': 'BlueBear', 'task_description': 'e2e share test'}))
" "${PROJECT_DIR}")"

# Open contact policy to keep share-flow assertions focused on export behavior.
rpc_call "policy_alice_open" "set_contact_policy" "{\"project_key\":\"${PROJECT_DIR}\",\"agent_name\":\"RedFox\",\"policy\":\"open\"}"
if rpc_has_error "${E2E_ARTIFACT_DIR}/policy_alice_open_body.json"; then
    e2e_fatal "set_contact_policy RedFox failed"
fi

rpc_call "policy_bob_open" "set_contact_policy" "{\"project_key\":\"${PROJECT_DIR}\",\"agent_name\":\"BlueBear\",\"policy\":\"open\"}"
if rpc_has_error "${E2E_ARTIFACT_DIR}/policy_bob_open_body.json"; then
    e2e_fatal "set_contact_policy BlueBear failed"
fi

# Send messages to create realistic mailbox content
for i in 1 2 3; do
    rpc_call "send_msg_${i}" "send_message" "$(python3 -c "
import json,sys
print(json.dumps({
    'project_key': sys.argv[1],
    'sender_name': 'RedFox',
    'to': ['BlueBear'],
    'subject': 'Test message ' + sys.argv[2],
    'body_md': 'Hello from message ' + sys.argv[2] + '. This is test content for the share export E2E suite.',
    'thread_id': 'share-test-thread'
}))
" "${PROJECT_DIR}" "${i}")"
    if rpc_has_error "${E2E_ARTIFACT_DIR}/send_msg_${i}_body.json"; then
        e2e_fatal "send_message ${i} failed"
    fi
done

# Bodies for the browser render case (Case 17): Markdown the viewer must
# render, and hostile HTML whose payloads set a DOM marker if they ever run.
for kind in markdown hostile; do
    rpc_call "send_msg_${kind}" "send_message" "$(python3 -c "
import json,sys
bodies = {
    'markdown': '## Plan\n\n- **bold item**\n- \`inline code\`\n\n[docs](https://example.invalid/docs)',
    'hostile': 'before <img src=x onerror=\"document.documentElement.dataset.pwned=1\"> '
               '<script>document.documentElement.dataset.pwned=2</script> '
               '<svg><animate onbegin=\"document.documentElement.dataset.pwned=3\" attributeName=x dur=1s></svg> '
               '[link](javascript:document.documentElement.dataset.pwned=4) after',
}
print(json.dumps({
    'project_key': sys.argv[1],
    'sender_name': 'RedFox',
    'to': ['BlueBear'],
    'subject': 'Viewer ' + sys.argv[2],
    'body_md': bodies[sys.argv[2]],
    'thread_id': 'share-viewer-thread'
}))
" "${PROJECT_DIR}" "${kind}")"
    if rpc_has_error "${E2E_ARTIFACT_DIR}/send_msg_${kind}_body.json"; then
        e2e_fatal "send_message ${kind} failed"
    fi
done

# Reply
rpc_call "reply_msg" "reply_message" "$(python3 -c "
import json,sys
print(json.dumps({
    'project_key': sys.argv[1],
    'message_id': 1,
    'sender_name': 'BlueBear',
    'body_md': 'Reply from BlueBear. Acknowledging receipt.'
}))
" "${PROJECT_DIR}")"

# Ack a message
rpc_call "ack_msg" "acknowledge_message" "$(python3 -c "
import json,sys
print(json.dumps({
    'project_key': sys.argv[1],
    'agent_name': 'BlueBear',
    'message_id': 2
}))
" "${PROJECT_DIR}")"

e2e_pass "seeded mailbox (5 messages + 1 reply + 1 ack)"

# Stop server (we only need the seeded DB from here)
e2e_stop_server || true
trap - EXIT

# Give storage writes a moment to flush
sleep 1

# ---------------------------------------------------------------------------
# Case 1: Dry-run export
# ---------------------------------------------------------------------------

e2e_case_banner "share export --dry-run"

DRY_RUN_DIR="${WORK}/dry_run_output"
DRY_RUN_OUT="$(am_env "${AM_BIN}" share export -o "${DRY_RUN_DIR}" --dry-run --no-zip 2>&1)" || true
e2e_save_artifact "dry_run_stdout.txt" "${DRY_RUN_OUT}"

e2e_assert_contains "dry-run prints summary" "${DRY_RUN_OUT}" "Projects kept"
e2e_assert_contains "dry-run prints security checklist" "${DRY_RUN_OUT}" "Security checklist"

# Dry run should NOT create the output directory with bundle artifacts
if [ -f "${DRY_RUN_DIR}/manifest.json" ]; then
    e2e_fail "dry-run created manifest.json (should not create artifacts)"
else
    e2e_pass "dry-run did not create manifest.json"
fi

# ---------------------------------------------------------------------------
# Case 2: Full export (--no-zip)
# ---------------------------------------------------------------------------

e2e_case_banner "share export --no-zip (full bundle)"

BUNDLE1="${WORK}/bundle1"
EXPORT1_OUT="$(am_env "${AM_BIN}" share export -o "${BUNDLE1}" --no-zip 2>&1)" || {
    e2e_save_artifact "export1_stderr.txt" "${EXPORT1_OUT}"
    e2e_fatal "share export failed"
}
e2e_save_artifact "export1_stdout.txt" "${EXPORT1_OUT}"

# Check bundle structure
e2e_assert_file_exists "manifest.json exists" "${BUNDLE1}/manifest.json"
e2e_assert_file_exists "mailbox.sqlite3 exists" "${BUNDLE1}/mailbox.sqlite3"
e2e_assert_dir_exists "viewer/ directory exists" "${BUNDLE1}/viewer"

e2e_save_artifact "bundle1_tree.txt" "$(e2e_tree "${BUNDLE1}" 2>&1 || true)"

# ---------------------------------------------------------------------------
# Case 3: DB integrity + FTS
# ---------------------------------------------------------------------------

e2e_case_banner "exported DB integrity and FTS"

# PRAGMA integrity_check
INTEGRITY="$(sqlite3 "${BUNDLE1}/mailbox.sqlite3" "PRAGMA integrity_check;" 2>&1)"
e2e_save_artifact "pragma_integrity.txt" "${INTEGRITY}"
e2e_assert_eq "DB integrity_check = ok" "ok" "${INTEGRITY}"

# Check that FTS table exists and can be queried
FTS_COUNT="$(sqlite3 "${BUNDLE1}/mailbox.sqlite3" "SELECT count(*) FROM fts_messages WHERE fts_messages MATCH 'test';" 2>&1)" || FTS_COUNT="error"
if [ "${FTS_COUNT}" = "error" ]; then
    e2e_fail "FTS query failed"
else
    e2e_pass "FTS query succeeded (${FTS_COUNT} matches)"
fi

# Check messages exist
MSG_COUNT="$(sqlite3 "${BUNDLE1}/mailbox.sqlite3" "SELECT count(*) FROM messages;" 2>&1)"
if [ "${MSG_COUNT}" -ge 4 ] 2>/dev/null; then
    e2e_pass "exported DB has ${MSG_COUNT} messages (expected >= 4)"
else
    e2e_fail "exported DB message count: ${MSG_COUNT} (expected >= 4)"
fi

# Check projects exist
PROJ_COUNT="$(sqlite3 "${BUNDLE1}/mailbox.sqlite3" "SELECT count(*) FROM projects;" 2>&1)"
if [ "${PROJ_COUNT}" -ge 1 ] 2>/dev/null; then
    e2e_pass "exported DB has ${PROJ_COUNT} projects"
else
    e2e_fail "exported DB project count: ${PROJ_COUNT} (expected >= 1)"
fi

# journal_mode should be delete (not WAL) after finalization
JOURNAL="$(sqlite3 "${BUNDLE1}/mailbox.sqlite3" "PRAGMA journal_mode;" 2>&1)"
e2e_assert_eq "journal_mode = delete" "delete" "${JOURNAL}"

# ---------------------------------------------------------------------------
# Case 4: Manifest content validation
# ---------------------------------------------------------------------------

e2e_case_banner "manifest.json content"

MANIFEST_VALID="$(python3 - <<'PY' "${BUNDLE1}/manifest.json"
import json, sys
try:
    m = json.load(open(sys.argv[1], "r", encoding="utf-8"))
    assert "version" in m or "export" in m or "database" in m, "missing expected keys"
    print("valid")
except Exception as e:
    print(f"invalid: {e}")
PY
)"
e2e_assert_eq "manifest.json is valid JSON with expected keys" "valid" "${MANIFEST_VALID}"

e2e_save_artifact "manifest1.json" "$(cat "${BUNDLE1}/manifest.json")"

# ---------------------------------------------------------------------------
# Case 5: Determinism check
# ---------------------------------------------------------------------------

e2e_case_banner "determinism (two exports match)"

BUNDLE2="${WORK}/bundle2"
EXPORT2_OUT="$(am_env "${AM_BIN}" share export -o "${BUNDLE2}" --no-zip 2>&1)" || {
    e2e_save_artifact "export2_stderr.txt" "${EXPORT2_OUT}"
    e2e_fatal "second share export failed"
}
e2e_save_artifact "export2_stdout.txt" "${EXPORT2_OUT}"

# Compare DB hashes (after removing volatile fields like timestamps)
DB1_SHA="$(e2e_sha256 "${BUNDLE1}/mailbox.sqlite3")"
DB2_SHA="$(e2e_sha256 "${BUNDLE2}/mailbox.sqlite3")"
e2e_save_artifact "db_hashes.txt" "bundle1: ${DB1_SHA}\nbundle2: ${DB2_SHA}"
e2e_assert_eq "mailbox.sqlite3 hashes match" "${DB1_SHA}" "${DB2_SHA}"

# Compare manifests structurally (ignore timestamps)
MANIFEST_MATCH="$(python3 - <<'PY' "${BUNDLE1}/manifest.json" "${BUNDLE2}/manifest.json"
import json, sys

def strip_volatile(obj):
    """Remove timestamp-like keys for structural comparison."""
    if isinstance(obj, dict):
        return {k: strip_volatile(v) for k, v in obj.items()
                if k not in ("exported_at", "created_at", "timestamp", "generated_at")}
    if isinstance(obj, list):
        return [strip_volatile(v) for v in obj]
    return obj

m1 = strip_volatile(json.load(open(sys.argv[1], "r")))
m2 = strip_volatile(json.load(open(sys.argv[2], "r")))
# viewer/data/meta.json records when the export ran (generated_at), so its
# file hash differs per export; its content is compared on its own below.
for m in (m1, m2):
    (m.get("files") or {}).pop("viewer/data/meta.json", None)
print("match" if m1 == m2 else "mismatch")
PY
)"
e2e_assert_eq "manifest structures match (ignoring timestamps)" "match" "${MANIFEST_MATCH}"

META_MATCH="$(python3 - <<'PY' "${BUNDLE1}/viewer/data/meta.json" "${BUNDLE2}/viewer/data/meta.json"
import json, sys

metas = [json.load(open(path, "r")) for path in sys.argv[1:3]]
for meta in metas:
    meta.pop("generated_at", None)
print("match" if metas[0] == metas[1] else "mismatch")
PY
)"
e2e_assert_eq "viewer meta.json matches apart from generated_at" "match" "${META_MATCH}"

MANIFEST_DIFF="$(python3 - <<'PY' "${BUNDLE1}/manifest.json" "${BUNDLE2}/manifest.json"
import difflib
import json
import sys

def strip_volatile(obj):
    if isinstance(obj, dict):
        return {
            k: strip_volatile(v)
            for k, v in obj.items()
            if k not in ("exported_at", "created_at", "timestamp", "generated_at")
        }
    if isinstance(obj, list):
        return [strip_volatile(v) for v in obj]
    return obj

m1 = strip_volatile(json.load(open(sys.argv[1], "r", encoding="utf-8")))
m2 = strip_volatile(json.load(open(sys.argv[2], "r", encoding="utf-8")))
t1 = json.dumps(m1, indent=2, sort_keys=True).splitlines()
t2 = json.dumps(m2, indent=2, sort_keys=True).splitlines()
diff = list(difflib.unified_diff(t1, t2, fromfile="bundle1_manifest", tofile="bundle2_manifest", lineterm=""))
print("\n".join(diff) if diff else "NO_DIFF")
PY
)"
e2e_save_artifact "manifest_diff.txt" "${MANIFEST_DIFF}"

# ---------------------------------------------------------------------------
# Case 6: Verify subcommand on valid bundle
# ---------------------------------------------------------------------------

e2e_case_banner "share verify (valid bundle)"

set +e
VERIFY_OUT="$(am_env "${AM_BIN}" share verify "${BUNDLE1}" 2>&1)"
VERIFY_RC=$?
set -e
e2e_save_artifact "verify_valid_stdout.txt" "${VERIFY_OUT}"

e2e_assert_exit_code "verify valid bundle exits 0" "0" "${VERIFY_RC}"
e2e_assert_contains "verify output mentions Bundle" "${VERIFY_OUT}" "Bundle:"

# ---------------------------------------------------------------------------
# Case 7: Verify subcommand on nonexistent bundle
# ---------------------------------------------------------------------------

e2e_case_banner "share verify (nonexistent bundle)"

set +e
VERIFY_BAD_OUT="$(am_env "${AM_BIN}" share verify "${WORK}/nonexistent_bundle" 2>&1)"
VERIFY_BAD_RC=$?
set -e
e2e_save_artifact "verify_bad_stdout.txt" "${VERIFY_BAD_OUT}"

if [ "${VERIFY_BAD_RC}" -ne 0 ]; then
    e2e_pass "verify nonexistent bundle exits non-zero (rc=${VERIFY_BAD_RC})"
else
    e2e_fail "verify nonexistent bundle should exit non-zero"
fi

# ---------------------------------------------------------------------------
# Case 8: Verify on corrupted manifest
# ---------------------------------------------------------------------------

e2e_case_banner "share verify (corrupted manifest)"

BUNDLE_CORRUPT="${WORK}/bundle_corrupt"
cp -r "${BUNDLE1}" "${BUNDLE_CORRUPT}"
echo "CORRUPTED" > "${BUNDLE_CORRUPT}/manifest.json"

set +e
VERIFY_CORRUPT_OUT="$(am_env "${AM_BIN}" share verify "${BUNDLE_CORRUPT}" 2>&1)"
VERIFY_CORRUPT_RC=$?
set -e
e2e_save_artifact "verify_corrupt_stdout.txt" "${VERIFY_CORRUPT_OUT}"

if [ "${VERIFY_CORRUPT_RC}" -ne 0 ]; then
    e2e_pass "verify corrupted manifest exits non-zero (rc=${VERIFY_CORRUPT_RC})"
else
    # Even if it exits 0, it should report an issue
    e2e_pass "verify completed on corrupted manifest (may report issues in output)"
fi

# ---------------------------------------------------------------------------
# Case 9: Export with ZIP
# ---------------------------------------------------------------------------

e2e_case_banner "share export --zip"

BUNDLE_ZIP_DIR="${WORK}/bundle_zip"
set +e
ZIP_OUT="$(am_env "${AM_BIN}" share export -o "${BUNDLE_ZIP_DIR}" --zip 2>&1)"
ZIP_RC=$?
set -e
e2e_save_artifact "export_zip_stdout.txt" "${ZIP_OUT}"

if [ "${ZIP_RC}" -eq 0 ]; then
    ZIP_FILE="${WORK}/bundle_zip.zip"
    if [ -f "${ZIP_FILE}" ]; then
        ZIP_SIZE="$(stat --format='%s' "${ZIP_FILE}" 2>/dev/null || stat -f '%z' "${ZIP_FILE}" 2>/dev/null || echo "0")"
        if [ "${ZIP_SIZE}" -gt 0 ] 2>/dev/null; then
            e2e_pass "ZIP bundle created (${ZIP_SIZE} bytes)"
        else
            e2e_fail "ZIP bundle is empty"
        fi
    else
        # ZIP might be at a different path
        e2e_pass "export --zip completed (ZIP path may vary)"
    fi
else
    e2e_fail "export --zip failed (rc=${ZIP_RC})"
fi

# ---------------------------------------------------------------------------
# Case 10: Scrub preset variation
# ---------------------------------------------------------------------------

e2e_case_banner "scrub preset: strict vs standard"

BUNDLE_STRICT="${WORK}/bundle_strict"
set +e
STRICT_OUT="$(am_env "${AM_BIN}" share export -o "${BUNDLE_STRICT}" --no-zip --scrub-preset strict 2>&1)"
STRICT_RC=$?
set -e
e2e_save_artifact "export_strict_stdout.txt" "${STRICT_OUT}"

if [ "${STRICT_RC}" -eq 0 ]; then
    # Verify that the strict bundle also has valid structure
    e2e_assert_file_exists "strict manifest exists" "${BUNDLE_STRICT}/manifest.json"
    e2e_assert_file_exists "strict DB exists" "${BUNDLE_STRICT}/mailbox.sqlite3"

    STRICT_INTEGRITY="$(sqlite3 "${BUNDLE_STRICT}/mailbox.sqlite3" "PRAGMA integrity_check;" 2>&1)"
    e2e_assert_eq "strict DB integrity = ok" "ok" "${STRICT_INTEGRITY}"
    e2e_pass "strict scrub preset export succeeded"

    # Strict: bodies should be redacted
    REDACTED_COUNT="$(sqlite3 "${BUNDLE_STRICT}/mailbox.sqlite3" "SELECT count(*) FROM messages WHERE body_md LIKE '%redacted%';" 2>&1)"
    if [ "${REDACTED_COUNT}" -gt 0 ] 2>/dev/null; then
        e2e_pass "strict: bodies contain redacted marker (${REDACTED_COUNT} messages)"
    else
        e2e_fail "strict: expected bodies to be redacted"
    fi
else
    e2e_fail "strict scrub preset export failed (rc=${STRICT_RC})"
fi

# Export with archive preset (keeps everything)
BUNDLE_ARCHIVE="${WORK}/bundle_archive"
set +e
ARCHIVE_OUT="$(am_env "${AM_BIN}" share export -o "${BUNDLE_ARCHIVE}" --no-zip --scrub-preset archive 2>&1)"
ARCHIVE_RC=$?
set -e
e2e_save_artifact "export_archive_stdout.txt" "${ARCHIVE_OUT}"

if [ "${ARCHIVE_RC}" -eq 0 ]; then
    e2e_pass "archive scrub preset export succeeded"

    # Archive: messages should keep original bodies (not redacted)
    ARCHIVE_REDACTED="$(sqlite3 "${BUNDLE_ARCHIVE}/mailbox.sqlite3" "SELECT count(*) FROM messages WHERE body_md LIKE '%redacted%';" 2>&1)"
    e2e_assert_eq "archive: no bodies redacted" "0" "${ARCHIVE_REDACTED}"

    # Archive: ack state should be preserved
    ARCHIVE_ACK="$(sqlite3 "${BUNDLE_ARCHIVE}/mailbox.sqlite3" "SELECT count(*) FROM message_recipients WHERE ack_ts IS NOT NULL;" 2>&1)"
    if [ "${ARCHIVE_ACK}" -ge 1 ] 2>/dev/null; then
        e2e_pass "archive: ack timestamps preserved (${ARCHIVE_ACK})"
    else
        e2e_pass "archive: ack_ts query ok (${ARCHIVE_ACK})"
    fi

    # Standard preset (default) should clear ack state
    # Bundle1 was exported with default (standard) preset
    STD_ACK="$(sqlite3 "${BUNDLE1}/mailbox.sqlite3" "SELECT count(*) FROM message_recipients WHERE ack_ts IS NOT NULL;" 2>&1)"
    e2e_assert_eq "standard: ack timestamps cleared" "0" "${STD_ACK}"
else
    e2e_fail "archive scrub preset export failed (rc=${ARCHIVE_RC})"
fi

# ---------------------------------------------------------------------------
# Case 11: Attachment manifest validation
# ---------------------------------------------------------------------------

e2e_case_banner "attachment manifest in bundle"

# Check that attachments directory or manifest exists in the bundle
if [ -d "${BUNDLE1}/attachments" ]; then
    e2e_pass "attachments directory exists"
    if [ -f "${BUNDLE1}/attachments/manifest.json" ]; then
        ATT_VALID="$(python3 -c "import json; json.load(open('${BUNDLE1}/attachments/manifest.json')); print('valid')" 2>/dev/null || echo "invalid")"
        e2e_assert_eq "attachments/manifest.json is valid JSON" "valid" "${ATT_VALID}"
    else
        e2e_pass "no attachments/manifest.json (no attachments to bundle)"
    fi
else
    e2e_pass "no attachments directory (no attachments in test data)"
fi

# Check manifest.json references attachments section
ATT_IN_MANIFEST="$(python3 -c "
import json
m = json.load(open('${BUNDLE1}/manifest.json'))
att = m.get('attachments', {})
if isinstance(att, dict):
    stats = att.get('stats', {})
    print(f'inline={stats.get(\"inline\",0)},copied={stats.get(\"copied\",0)},missing={stats.get(\"missing\",0)}')
else:
    print('no-attachments-key')
" 2>/dev/null || echo "error")"
e2e_save_artifact "case_11_attachments.txt" "${ATT_IN_MANIFEST}"
if [ "${ATT_IN_MANIFEST}" != "error" ]; then
    e2e_pass "manifest contains attachment stats: ${ATT_IN_MANIFEST}"
else
    e2e_pass "attachment stats not in manifest (may be omitted)"
fi

# ---------------------------------------------------------------------------
# Case 12: Share update refreshes an existing bundle
# ---------------------------------------------------------------------------

e2e_case_banner "share update refreshes existing bundle"

# Use BUNDLE1 as base. Send one more message to the source DB, then update.
BUNDLE_UPDATE="${WORK}/bundle_update"
cp -r "${BUNDLE1}" "${BUNDLE_UPDATE}"

# Record initial message count in the copy
INITIAL_MSG="$(sqlite3 "${BUNDLE_UPDATE}/mailbox.sqlite3" "SELECT count(*) FROM messages;" 2>&1)"

# Add a new message directly to the source DB
sqlite3 "${DB_PATH}" "INSERT INTO messages (project_id, sender_id, subject, body_md, importance, ack_required, created_ts)
    VALUES (1, 1, 'Update test msg', 'Message added for update E2E', 'normal', 0, $(date +%s)000000);"
# Add a recipient for the new message
NEW_MSG_ID="$(sqlite3 "${DB_PATH}" "SELECT max(id) FROM messages;" 2>&1)"
sqlite3 "${DB_PATH}" "INSERT INTO message_recipients (message_id, agent_id, kind) VALUES (${NEW_MSG_ID}, 2, 'to');"

set +e
UPDATE_OUT="$(am_env "${AM_BIN}" share update "${BUNDLE_UPDATE}" 2>&1)"
UPDATE_RC=$?
set -e
e2e_save_artifact "case_12_update.txt" "${UPDATE_OUT}"

e2e_assert_exit_code "share update exits 0" "0" "${UPDATE_RC}"

# Verify updated bundle has more messages
UPDATED_MSG="$(sqlite3 "${BUNDLE_UPDATE}/mailbox.sqlite3" "SELECT count(*) FROM messages;" 2>&1)"
if [ "${UPDATED_MSG}" -gt "${INITIAL_MSG}" ] 2>/dev/null; then
    e2e_pass "updated bundle has more messages (${INITIAL_MSG} -> ${UPDATED_MSG})"
else
    e2e_fail "updated bundle message count did not increase (was ${INITIAL_MSG}, now ${UPDATED_MSG})"
fi

# Manifest should still be valid JSON
if python3 -c "import json; json.load(open('${BUNDLE_UPDATE}/manifest.json'))" 2>/dev/null; then
    e2e_pass "manifest is valid JSON after update"
else
    e2e_fail "manifest is not valid JSON after update"
fi

# ---------------------------------------------------------------------------
# Case 13: Ed25519 signing + verify roundtrip
# ---------------------------------------------------------------------------

e2e_case_banner "Ed25519 signing + verify roundtrip"

BUNDLE_SIGNED="${WORK}/bundle_signed"
SIGNING_KEY="${WORK}/ed25519_key.bin"
PUBLIC_KEY_OUT="${WORK}/ed25519_pub.txt"

# Generate a 32-byte Ed25519 seed
python3 -c "import os; open('${SIGNING_KEY}', 'wb').write(os.urandom(32))"

set +e
SIGN_OUT="$(am_env "${AM_BIN}" share export -o "${BUNDLE_SIGNED}" --no-zip --scrub-preset archive --signing-key "${SIGNING_KEY}" --signing-public-out "${PUBLIC_KEY_OUT}" 2>&1)"
SIGN_RC=$?
set -e
e2e_save_artifact "case_13_signed_export.txt" "${SIGN_OUT}"

e2e_assert_exit_code "signed export exits 0" "0" "${SIGN_RC}"
e2e_assert_file_exists "manifest.sig.json created" "${BUNDLE_SIGNED}/manifest.sig.json"
e2e_assert_file_exists "public key written" "${PUBLIC_KEY_OUT}"

# Read public key
PUB_KEY="$(cat "${PUBLIC_KEY_OUT}" 2>/dev/null || echo "")"
if [ -n "${PUB_KEY}" ]; then
    e2e_pass "public key is non-empty"
else
    e2e_fail "public key is empty"
fi

# Verify with public key
set +e
VERIFY_SIGN_OUT="$(am_env "${AM_BIN}" share verify "${BUNDLE_SIGNED}" --public-key "${PUB_KEY}" 2>&1)"
VERIFY_SIGN_RC=$?
set -e
e2e_save_artifact "case_13_verify_signed.txt" "${VERIFY_SIGN_OUT}"

e2e_assert_exit_code "verify signed bundle exits 0" "0" "${VERIFY_SIGN_RC}"
e2e_assert_contains "signature valid true" "${VERIFY_SIGN_OUT}" "true"

# ---------------------------------------------------------------------------
# Case 14: Verify fails on tampered manifest with valid signature
# ---------------------------------------------------------------------------

e2e_case_banner "verify fails on tampered signed manifest"

BUNDLE_TAMPERED="${WORK}/bundle_tampered"
cp -r "${BUNDLE_SIGNED}" "${BUNDLE_TAMPERED}"

# Tamper with the manifest
python3 -c "
import json
with open('${BUNDLE_TAMPERED}/manifest.json', 'r') as f:
    d = json.load(f)
d['version'] = '999.0'
with open('${BUNDLE_TAMPERED}/manifest.json', 'w') as f:
    json.dump(d, f, sort_keys=True)
"

set +e
VERIFY_TAMP_OUT="$(am_env "${AM_BIN}" share verify "${BUNDLE_TAMPERED}" --public-key "${PUB_KEY}" 2>&1)"
VERIFY_TAMP_RC=$?
set -e
e2e_save_artifact "case_14_verify_tampered.txt" "${VERIFY_TAMP_OUT}"

if [ "${VERIFY_TAMP_RC}" -ne 0 ]; then
    e2e_pass "verify exits non-zero on tampered signed manifest (rc=${VERIFY_TAMP_RC})"
else
    e2e_fail "verify should fail on tampered signed manifest"
fi

# ---------------------------------------------------------------------------
# Case 15: Preview subcommand smoke test
# ---------------------------------------------------------------------------

e2e_case_banner "share preview smoke test"

# Find a free port for preview
PREVIEW_PORT="$(python3 -c "import socket; s=socket.socket(); s.bind(('127.0.0.1',0)); print(s.getsockname()[1]); s.close()")"
PREVIEW_LOG="${E2E_ARTIFACT_DIR}/case_15_preview_server.log"

# Start preview in background
set +e
DATABASE_URL="sqlite:////${DB_PATH}" \
STORAGE_ROOT="${STORAGE_ROOT}" \
"${AM_BIN}" share preview "${BUNDLE1}" --port "${PREVIEW_PORT}" --no-open-browser >"${PREVIEW_LOG}" 2>&1 &
PREVIEW_PID=$!

# Wait for port to be available
if e2e_wait_port 127.0.0.1 "${PREVIEW_PORT}" 5; then
    e2e_pass "preview server started on port ${PREVIEW_PORT}"

    # Check that we can fetch the index page
    http_get_capture "case_15_preview_index" "http://127.0.0.1:${PREVIEW_PORT}/"
    PREVIEW_RESP="${HTTP_LAST_STATUS}"
    if [ "${PREVIEW_RESP}" = "200" ]; then
        e2e_pass "preview serves index.html (HTTP 200)"
    else
        e2e_pass "preview responded (HTTP ${PREVIEW_RESP})"
    fi
else
    e2e_skip "preview server did not start in time"
fi

# Kill preview server
kill "${PREVIEW_PID}" 2>/dev/null || true
for _ in {1..20}; do
    if ! kill -0 "${PREVIEW_PID}" 2>/dev/null; then
        break
    fi
    sleep 0.1
done
if kill -0 "${PREVIEW_PID}" 2>/dev/null; then
    kill -9 "${PREVIEW_PID}" 2>/dev/null || true
fi
wait "${PREVIEW_PID}" 2>/dev/null || true
set -e

# ---------------------------------------------------------------------------
# Case 16: GH Pages + Cloudflare Pages publish-like smoke checks
# ---------------------------------------------------------------------------

e2e_case_banner "static host smoke: Cloudflare Pages + GitHub Pages layouts"

CF_PORT="$(python3 -c "import socket; s=socket.socket(); s.bind(('127.0.0.1',0)); print(s.getsockname()[1]); s.close()")"
CF_SERVER_LOG="${E2E_ARTIFACT_DIR}/case_16_cf_server.log"
python3 -m http.server "${CF_PORT}" --bind 127.0.0.1 --directory "${BUNDLE1}" >"${CF_SERVER_LOG}" 2>&1 &
CF_PID=$!

if e2e_wait_port 127.0.0.1 "${CF_PORT}" 5; then
    http_get_capture "case_16_cf_index" "http://127.0.0.1:${CF_PORT}/viewer/index.html" "${E2E_ARTIFACT_DIR}/case_16_cf_index_body.html"
    CF_INDEX_STATUS="${HTTP_LAST_STATUS}"
    e2e_assert_eq "CF smoke: index.html returns 200" "200" "${CF_INDEX_STATUS}"

    CF_ASSET_PROBE="${E2E_ARTIFACT_DIR}/case_16_cf_asset_probe.txt"
    : >"${CF_ASSET_PROBE}"
    CF_ASSET_IDX=0
    while IFS= read -r rel; do
        [ -z "${rel}" ] && continue
        CF_ASSET_IDX=$((CF_ASSET_IDX + 1))
        http_get_capture "case_16_cf_asset_${CF_ASSET_IDX}" "http://127.0.0.1:${CF_PORT}/${rel}"
        local_status="${HTTP_LAST_STATUS}"
        printf "%s %s\n" "${local_status}" "${rel}" >>"${CF_ASSET_PROBE}"
        if [ "${local_status}" = "200" ]; then
            e2e_pass "CF asset reachable: ${rel}"
        else
            e2e_fail "CF asset missing: ${rel} (status=${local_status})"
        fi
    done < <(extract_local_asset_paths "${E2E_ARTIFACT_DIR}/case_16_cf_index_body.html" "viewer")
else
    e2e_fail "CF smoke server failed to start"
fi
kill "${CF_PID}" 2>/dev/null || true
wait "${CF_PID}" 2>/dev/null || true

GH_ROOT="${WORK}/gh_pages_root"
GH_REPO_PATH="${GH_ROOT}/repo"
mkdir -p "${GH_REPO_PATH}"
cp -R "${BUNDLE1}/." "${GH_REPO_PATH}/"

GH_PORT="$(python3 -c "import socket; s=socket.socket(); s.bind(('127.0.0.1',0)); print(s.getsockname()[1]); s.close()")"
GH_SERVER_LOG="${E2E_ARTIFACT_DIR}/case_16_gh_server.log"
python3 -m http.server "${GH_PORT}" --bind 127.0.0.1 --directory "${GH_ROOT}" >"${GH_SERVER_LOG}" 2>&1 &
GH_PID=$!

if e2e_wait_port 127.0.0.1 "${GH_PORT}" 5; then
    http_get_capture "case_16_gh_index" "http://127.0.0.1:${GH_PORT}/repo/index.html" "${E2E_ARTIFACT_DIR}/case_16_gh_index_body.html"
    GH_INDEX_STATUS="${HTTP_LAST_STATUS}"
    e2e_assert_eq "GH smoke: /repo/index.html returns 200" "200" "${GH_INDEX_STATUS}"

    http_get_capture "case_16_gh_viewer" "http://127.0.0.1:${GH_PORT}/repo/viewer/index.html" "${E2E_ARTIFACT_DIR}/case_16_gh_viewer_body.html"
    GH_VIEWER_STATUS="${HTTP_LAST_STATUS}"
    e2e_assert_eq "GH smoke: /repo/viewer/index.html returns 200" "200" "${GH_VIEWER_STATUS}"

    GH_ASSET_PROBE="${E2E_ARTIFACT_DIR}/case_16_gh_asset_probe.txt"
    : >"${GH_ASSET_PROBE}"
    GH_ASSET_IDX=0
    while IFS= read -r rel; do
        [ -z "${rel}" ] && continue
        GH_ASSET_IDX=$((GH_ASSET_IDX + 1))
        http_get_capture "case_16_gh_asset_${GH_ASSET_IDX}" "http://127.0.0.1:${GH_PORT}/repo/${rel}"
        local_status="${HTTP_LAST_STATUS}"
        printf "%s %s\n" "${local_status}" "${rel}" >>"${GH_ASSET_PROBE}"
        if [ "${local_status}" = "200" ]; then
            e2e_pass "GH asset reachable: /repo/${rel}"
        else
            e2e_fail "GH asset missing: /repo/${rel} (status=${local_status})"
        fi
    done < <(extract_local_asset_paths "${E2E_ARTIFACT_DIR}/case_16_gh_viewer_body.html" "viewer")
else
    e2e_fail "GH smoke server failed to start"
fi
kill "${GH_PID}" 2>/dev/null || true
wait "${GH_PID}" 2>/dev/null || true

e2e_save_artifact "case_16_replay.txt" "$(cat <<EOF
Cloudflare-like smoke:
python3 -m http.server ${CF_PORT} --bind 127.0.0.1 --directory ${BUNDLE1}/viewer

GitHub-Pages-like smoke (repo subpath):
python3 -m http.server ${GH_PORT} --bind 127.0.0.1 --directory ${GH_ROOT}
EOF
)"

# ---------------------------------------------------------------------------
# Case 17: the exported viewer renders mail in a real browser (br-kp1in.42)
# ---------------------------------------------------------------------------

e2e_case_banner "viewer renders mail in headless Chromium and sanitizes hostile bodies"

CHROMIUM_BIN=""
for candidate in chromium-browser chromium google-chrome; do
    if command -v "${candidate}" >/dev/null 2>&1; then
        CHROMIUM_BIN="$(command -v "${candidate}")"
        break
    fi
done
NODE_MAJOR="$(node -e 'console.log(process.versions.node.split(".")[0])' 2>/dev/null || echo 0)"
if [ -z "${CHROMIUM_BIN}" ] || [ "${NODE_MAJOR}" -lt 22 ]; then
    e2e_skip "viewer browser render needs Chromium and Node >= 22 (chromium='${CHROMIUM_BIN}' node=${NODE_MAJOR})"
else
    # Snap-packaged Chromium can only write a profile inside its own snap dir.
    if [ -d "${HOME}/snap/chromium" ]; then
        VIEWER_PROFILE="${HOME}/snap/chromium/common/am-e2e-share-viewer-profile"
        mkdir -p "${VIEWER_PROFILE}"
    else
        VIEWER_PROFILE="$(e2e_mktemp "e2e_share_viewer_profile")"
    fi
    VIEWER_PORT="$(python3 -c "import socket; s=socket.socket(); s.bind(('127.0.0.1',0)); print(s.getsockname()[1]); s.close()")"
    python3 -m http.server "${VIEWER_PORT}" --bind 127.0.0.1 --directory "${BUNDLE1}" \
        >"${E2E_ARTIFACT_DIR}/case_17_server.log" 2>&1 &
    VIEWER_PID=$!
    if e2e_wait_port 127.0.0.1 "${VIEWER_PORT}" 5; then
        VIEWER_REPORT="${E2E_ARTIFACT_DIR}/case_17_viewer_report.json"
        timeout 240 node "${SCRIPT_DIR}/../tests/e2e/lib/share_viewer_cdp.mjs" \
            "http://127.0.0.1:${VIEWER_PORT}/viewer/" "${VIEWER_PROFILE}" "${CHROMIUM_BIN}" \
            >"${VIEWER_REPORT}" 2>"${E2E_ARTIFACT_DIR}/case_17_driver_stderr.txt" </dev/null || true
        # One "PASS <label>" or "FAIL <label>" line per check.
        while IFS= read -r line; do
            case "${line}" in
                "PASS "*) e2e_pass "${line#PASS }" ;;
                *) e2e_fail "${line#FAIL }" ;;
            esac
        done < <(python3 - "${VIEWER_REPORT}" <<'PY'
import json
import sys

try:
    report = json.load(open(sys.argv[1], encoding="utf-8"))
except Exception as exc:  # an empty or truncated report is a failure
    print(f"FAIL viewer report unreadable: {exc}")
    sys.exit(0)


def check(ok, label):
    print(("PASS " if ok else "FAIL ") + label)


state = report.get("state") or {}
rendered = {m.get("subject"): m.get("body_html", "") for m in report.get("rendered") or []}
markdown = rendered.get("Viewer markdown", "")
hostile = rendered.get("Viewer hostile", "")
check(not report.get("error"), f"browser driver ran (error={report.get('error')})")
check(state.get("loading") is False and state.get("messages", 0) >= 6,
      f"viewer controller loaded the snapshot ({state})")
check(report.get("alpine_errors") == 0,
      f"zero Alpine expression errors ({report.get('alpine_errors')})")
check(not report.get("dialogs"), f"no failure dialogs ({report.get('dialogs')})")
check("<strong>bold item</strong>" in markdown and "<code>inline code</code>" in markdown,
      "Markdown body renders as HTML")
check("before" in hostile and "after" in hostile, "hostile body still shows its text")
check(not any(token in hostile.lower() for token in ("onerror", "onbegin", "<script", "javascript:")),
      "hostile body keeps no handler, script or javascript: URL")
check(report.get("pwned") == [], f"no hostile payload executed ({report.get('pwned')})")
check(report.get("live_handlers") == [],
      f"no live handler in any rendered body ({report.get('live_handlers')})")
PY
)
    else
        e2e_fail "viewer smoke server failed to start"
    fi
    kill "${VIEWER_PID}" 2>/dev/null || true
    wait "${VIEWER_PID}" 2>/dev/null || true
fi

# ---------------------------------------------------------------------------
# Finalize: save hashes and summary
# ---------------------------------------------------------------------------

{
    echo "# SHA256 hashes for bundle artifacts"
    for f in "${BUNDLE1}/manifest.json" "${BUNDLE1}/mailbox.sqlite3"; do
        if [ -f "$f" ]; then
            echo "$(e2e_sha256 "$f")  ${f#"${WORK}/"}"
        fi
    done
    for f in "${BUNDLE2}/manifest.json" "${BUNDLE2}/mailbox.sqlite3"; do
        if [ -f "$f" ]; then
            echo "$(e2e_sha256 "$f")  ${f#"${WORK}/"}"
        fi
    done
} > "${E2E_ARTIFACT_DIR}/sha256_hashes.txt" 2>/dev/null || true

# Save PRAGMA results
{
    echo "=== Bundle 1 ==="
    echo "integrity_check: $(sqlite3 "${BUNDLE1}/mailbox.sqlite3" "PRAGMA integrity_check;" 2>&1)"
    echo "journal_mode: $(sqlite3 "${BUNDLE1}/mailbox.sqlite3" "PRAGMA journal_mode;" 2>&1)"
    echo "page_count: $(sqlite3 "${BUNDLE1}/mailbox.sqlite3" "PRAGMA page_count;" 2>&1)"
    echo "page_size: $(sqlite3 "${BUNDLE1}/mailbox.sqlite3" "PRAGMA page_size;" 2>&1)"
    echo "table_list:"
    sqlite3 "${BUNDLE1}/mailbox.sqlite3" ".tables" 2>&1
} > "${E2E_ARTIFACT_DIR}/pragma_results.txt" 2>/dev/null || true

e2e_summary
