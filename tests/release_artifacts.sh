#!/bin/sh

set -eu

[ "$#" -ge 2 ] || {
    echo "usage: tests/release_artifacts.sh DIRECTORY VERSION [COMMIT] [--require-signatures] [--metadata-only]" >&2
    exit 2
}

directory=$1
version=$2
shift 2
commit=
if [ "$#" -gt 0 ] && [ "${1#--}" = "$1" ]; then
    commit=$1
    shift
fi
require_signatures=false
metadata_only=false
for option in "$@"; do
    case $option in
        --require-signatures) require_signatures=true ;;
        --metadata-only) metadata_only=true ;;
        *) echo "unknown option: $option" >&2; exit 2 ;;
    esac
done
[ -d "$directory" ] || { echo "release directory does not exist: $directory" >&2; exit 1; }

export RAMIZ_RELEASE_DIRECTORY="$directory"
export RAMIZ_RELEASE_VERSION="$version"
export RAMIZ_RELEASE_COMMIT="$commit"
export RAMIZ_RELEASE_REQUIRE_SIGNATURES="$require_signatures"
export RAMIZ_RELEASE_METADATA_ONLY="$metadata_only"
python3 - <<'PY'
import hashlib
import json
import os
import platform
import subprocess
import tarfile
import tempfile
from pathlib import Path

directory = Path(os.environ["RAMIZ_RELEASE_DIRECTORY"])
version = os.environ["RAMIZ_RELEASE_VERSION"]
commit = os.environ["RAMIZ_RELEASE_COMMIT"]
require_signatures = os.environ["RAMIZ_RELEASE_REQUIRE_SIGNATURES"] == "true"
metadata_only = os.environ["RAMIZ_RELEASE_METADATA_ONLY"] == "true"
archives = {
    f"ramiz_{version}_darwin_arm64.tar.gz",
    f"ramiz_{version}_darwin_amd64.tar.gz",
    f"ramiz_{version}_linux_arm64.tar.gz",
    f"ramiz_{version}_linux_amd64.tar.gz",
}
actual = {path.name for path in directory.iterdir() if path.is_file()}
allowed = archives | {"checksums.txt", "checksums.txt.minisig", "attestation.jsonl"}
if actual - allowed or archives - actual or "checksums.txt" not in actual:
    raise SystemExit(f"unexpected release asset set: {sorted(actual)}")

checksums = {}
for line in (directory / "checksums.txt").read_text().splitlines():
    fields = line.split("  ")
    if len(fields) != 2 or fields[1] in checksums:
        raise SystemExit("malformed or duplicate checksum entry")
    digest, name = fields
    if len(digest) != 64 or any(char not in "0123456789abcdef" for char in digest):
        raise SystemExit(f"invalid checksum for {name}")
    if name not in archives:
        raise SystemExit(f"unexpected checksum entry: {name}")
    checksums[name] = digest
if set(checksums) != archives:
    raise SystemExit("checksums.txt does not cover exactly four archives")
for name, digest in checksums.items():
    if hashlib.sha256((directory / name).read_bytes()).hexdigest() != digest:
        raise SystemExit(f"checksum mismatch: {name}")
signature = directory / "checksums.txt.minisig"
public_key = Path("release/minisign.pub")
if signature.exists():
    if not public_key.is_file():
        raise SystemExit("checksums signature is present but release/minisign.pub is missing")
    try:
        subprocess.run(["minisign", "-Vm", str(directory / "checksums.txt"), "-p", str(public_key), "-x", str(signature)], check=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    except FileNotFoundError as error:
        raise SystemExit("minisign is required to verify checksums.txt.minisig") from error
    except subprocess.CalledProcessError as error:
        raise SystemExit(f"minisign verification failed: {error.stderr.strip()}") from error

native = None if metadata_only else {"Darwin": "darwin", "Linux": "linux"}.get(platform.system())
machine = platform.machine()
native_arch = None if metadata_only else {"arm64": "arm64", "aarch64": "arm64", "x86_64": "amd64"}.get(machine)
with tempfile.TemporaryDirectory(prefix="ramiz-release-smoke-") as temporary:
    for name in sorted(archives):
        with tarfile.open(directory / name, "r:gz") as archive:
            members = archive.getmembers()
            expected_members = {"ramiz", "git-ramiz", "README.md", "LICENSE", "NOTICE", "SOURCE.json", "ramiz-manifest.json"}
            member_names = {member.name for member in members}
            if "ramiz-manifest.json.minisig" in member_names:
                expected_members.add("ramiz-manifest.json.minisig")
            if member_names != expected_members or len(members) != len(expected_members):
                raise SystemExit(f"invalid members in {name}")
            for member in members:
                if not member.isfile() or member.uid != 0 or member.gid != 0 or member.mtime != 0:
                    raise SystemExit(f"non-deterministic metadata in {name}:{member.name}")
                if member.mode != (0o755 if member.name in ("ramiz", "git-ramiz") else 0o644):
                    raise SystemExit(f"invalid mode in {name}:{member.name}")
            source = json.loads(archive.extractfile("SOURCE.json").read())
            if source["version"] != version or (commit and source["commit"] != commit):
                raise SystemExit(f"SOURCE.json provenance mismatch in {name}")
            target_by_archive = {
                "darwin_arm64": "aarch64-apple-darwin",
                "darwin_amd64": "x86_64-apple-darwin",
                "linux_arm64": "aarch64-unknown-linux-musl",
                "linux_amd64": "x86_64-unknown-linux-musl",
            }
            archive_key = name.removeprefix(f"ramiz_{version}_").removesuffix(".tar.gz")
            if source["target"] != target_by_archive.get(archive_key):
                raise SystemExit(f"invalid target in {name}")
            manifest_bytes = archive.extractfile("ramiz-manifest.json").read()
            manifest = json.loads(manifest_bytes)
            expected_manifest = {
                "binaries": {
                    "git-ramiz": {"path": "git-ramiz", "sha256": hashlib.sha256(archive.extractfile("git-ramiz").read()).hexdigest()},
                    "ramiz": {"path": "ramiz", "sha256": hashlib.sha256(archive.extractfile("ramiz").read()).hexdigest()},
                },
                "commit": source["commit"],
                "repository": "agensfield/ramiz",
                "schema": "ramiz.manifest/v1",
                "target": source["target"],
                "version": version,
            }
            canonical_manifest = (json.dumps(expected_manifest, separators=(",", ":"), sort_keys=True) + "\n").encode()
            if manifest != expected_manifest or manifest_bytes != canonical_manifest:
                raise SystemExit(f"manifest binding mismatch in {name}")
            if require_signatures and "ramiz-manifest.json.minisig" not in member_names:
                raise SystemExit(f"manifest signature missing in {name}")
            if "ramiz-manifest.json.minisig" in member_names:
                with tempfile.TemporaryDirectory(prefix="ramiz-manifest-verify-") as verify_dir:
                    manifest_path = Path(verify_dir) / "ramiz-manifest.json"
                    signature_path = Path(verify_dir) / "ramiz-manifest.json.minisig"
                    manifest_path.write_bytes(manifest_bytes)
                    signature_path.write_bytes(archive.extractfile("ramiz-manifest.json.minisig").read())
                    try:
                        subprocess.run(["minisign", "-Vm", str(manifest_path), "-p", "release/minisign.pub", "-x", str(signature_path)], check=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
                    except (FileNotFoundError, subprocess.CalledProcessError) as error:
                        raise SystemExit(f"manifest signature verification failed in {name}") from error
            if native == name.split("_")[2] and native_arch == name.split("_")[3].split(".")[0]:
                for binary in ("ramiz", "git-ramiz"):
                    path = Path(temporary) / binary
                    path.write_bytes(archive.extractfile(binary).read())
                    path.chmod(0o700)
                    output = subprocess.check_output([str(path), "--version"], text=True).strip()
                    if output != f"ramiz {version}":
                        raise SystemExit(f"{binary} reported {output!r}, expected ramiz {version!r}")
print(json.dumps({"archives": 4, "checksums": True, "dual_binary_smoke": native is not None and native_arch is not None}))
PY
