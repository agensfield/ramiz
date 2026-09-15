#!/bin/sh

set -eu

usage() {
    cat >&2 <<'EOF'
usage:
  scripts/package.sh archive --target TARGET --binary-dir DIR --output DIR --version VERSION --commit SHA [--signing-key-file FILE]
  scripts/package.sh checksums --directory DIR
EOF
    exit 2
}

die() {
    echo "package.sh: $*" >&2
    exit 1
}

[ "$#" -ge 1 ] || usage
command=$1
shift

target=
binary_dir=
output_dir=
version=
commit=
directory=
signing_key_file=

while [ "$#" -gt 0 ]; do
    case $1 in
        --target)
            [ "$#" -ge 2 ] || usage
            target=$2
            shift 2
            ;;
        --binary-dir)
            [ "$#" -ge 2 ] || usage
            binary_dir=$2
            shift 2
            ;;
        --output)
            [ "$#" -ge 2 ] || usage
            output_dir=$2
            shift 2
            ;;
        --version)
            [ "$#" -ge 2 ] || usage
            version=$2
            shift 2
            ;;
        --commit)
            [ "$#" -ge 2 ] || usage
            commit=$2
            shift 2
            ;;
        --signing-key-file)
            [ "$#" -ge 2 ] || usage
            signing_key_file=$2
            shift 2
            ;;
        --directory)
            [ "$#" -ge 2 ] || usage
            directory=$2
            shift 2
            ;;
        *)
            usage
            ;;
    esac
done

case $command in
    archive)
        [ -n "$target" ] && [ -n "$binary_dir" ] && [ -n "$output_dir" ] &&
            [ -n "$version" ] && [ -n "$commit" ] || usage
        case $target in
            aarch64-apple-darwin) platform=darwin; arch=arm64 ;;
            x86_64-apple-darwin) platform=darwin; arch=amd64 ;;
            aarch64-unknown-linux-musl) platform=linux; arch=arm64 ;;
            x86_64-unknown-linux-musl) platform=linux; arch=amd64 ;;
            *) die "unsupported target: $target" ;;
        esac
        case $version in
            ''|*[!0-9A-Za-z.-]*) die "invalid version: $version" ;;
        esac
        [ "${#commit}" -eq 40 ] || die "commit must be a 40-character hexadecimal SHA"
        case $commit in
            *[!0123456789abcdef]*) die "commit must be a lowercase hexadecimal SHA" ;;
        esac
        [ -d "$binary_dir" ] || die "binary directory does not exist: $binary_dir"
        [ -f "$binary_dir/ramiz" ] || die "missing $binary_dir/ramiz"
        [ -f "$binary_dir/git-ramiz" ] || die "missing $binary_dir/git-ramiz"
        if [ -n "$signing_key_file" ] && [ ! -f "$signing_key_file" ]; then
            die "signing key does not exist: $signing_key_file"
        fi
        [ -f LICENSE ] && [ -f README.md ] && [ -f NOTICE ] ||
            die "run from the repository root with LICENSE, README.md, and NOTICE"
        mkdir -p "$output_dir"
        archive="$output_dir/ramiz_${version}_${platform}_${arch}.tar.gz"
        [ ! -e "$archive" ] || die "refusing to replace $archive"
        export RAMIZ_PACKAGE_TARGET="$target"
        export RAMIZ_PACKAGE_BINARY_DIR="$binary_dir"
        export RAMIZ_PACKAGE_OUTPUT="$archive"
        export RAMIZ_PACKAGE_VERSION="$version"
        export RAMIZ_PACKAGE_COMMIT="$commit"
        export RAMIZ_PACKAGE_SIGNING_KEY="$signing_key_file"
        python3 - <<'PY'
import gzip
import hashlib
import io
import json
import os
import subprocess
import tempfile
import tarfile

target = os.environ["RAMIZ_PACKAGE_TARGET"]
binary_dir = os.environ["RAMIZ_PACKAGE_BINARY_DIR"]
output = os.environ["RAMIZ_PACKAGE_OUTPUT"]
version = os.environ["RAMIZ_PACKAGE_VERSION"]
commit = os.environ["RAMIZ_PACKAGE_COMMIT"]

source = {
    "commit": commit,
    "repository": "https://github.com/agensfield/ramiz",
    "source": f"https://github.com/agensfield/ramiz/tree/{commit}",
    "target": target,
    "version": version,
}
binary_data = {
    name: open(os.path.join(binary_dir, name), "rb").read()
    for name in ("ramiz", "git-ramiz")
}
manifest = {
    "binaries": {
        "git-ramiz": {"path": "git-ramiz", "sha256": hashlib.sha256(binary_data["git-ramiz"]).hexdigest()},
        "ramiz": {"path": "ramiz", "sha256": hashlib.sha256(binary_data["ramiz"]).hexdigest()},
    },
    "commit": commit,
    "repository": "agensfield/ramiz",
    "schema": "ramiz.manifest/v1",
    "target": target,
    "version": version,
}
manifest_bytes = (json.dumps(manifest, separators=(",", ":"), sort_keys=True) + "\n").encode()
signature_bytes = None
signing_key = os.environ["RAMIZ_PACKAGE_SIGNING_KEY"]
if signing_key:
    with tempfile.TemporaryDirectory(prefix="ramiz-manifest-") as temporary:
        manifest_path = os.path.join(temporary, "ramiz-manifest.json")
        signature_path = os.path.join(temporary, "ramiz-manifest.json.minisig")
        open(manifest_path, "wb").write(manifest_bytes)
        subprocess.run(["minisign", "-Sm", manifest_path, "-s", signing_key, "-x", signature_path, "-t", "ramiz-manifest-v1"], check=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        subprocess.run(["minisign", "-Vm", manifest_path, "-p", "release/minisign.pub", "-x", signature_path], check=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        signature_bytes = open(signature_path, "rb").read()
members = [
    ("ramiz", binary_data["ramiz"], 0o755),
    ("git-ramiz", binary_data["git-ramiz"], 0o755),
    ("README.md", "README.md", 0o644),
    ("LICENSE", "LICENSE", 0o644),
    ("NOTICE", "NOTICE", 0o644),
    ("SOURCE.json", (json.dumps(source, indent=2, sort_keys=True) + "\n").encode(), 0o644),
    ("ramiz-manifest.json", manifest_bytes, 0o644),
]
if signature_bytes is not None:
    members.append(("ramiz-manifest.json.minisig", signature_bytes, 0o644))

with open(output, "wb") as raw:
    with gzip.GzipFile(fileobj=raw, mode="wb", filename="", mtime=0, compresslevel=9) as compressed:
        with tarfile.open(fileobj=compressed, mode="w", format=tarfile.USTAR_FORMAT) as archive:
            for name, path, mode in members:
                data = path if isinstance(path, bytes) else open(path, "rb").read()
                info = tarfile.TarInfo(name)
                info.size = len(data)
                info.mode = mode
                info.mtime = 0
                info.uid = 0
                info.gid = 0
                info.uname = ""
                info.gname = ""
                archive.addfile(info, io.BytesIO(data))
PY
        ;;
    checksums)
        [ -n "$directory" ] || usage
        [ -d "$directory" ] || die "directory does not exist: $directory"
        export RAMIZ_PACKAGE_CHECKSUM_DIR="$directory"
        python3 - <<'PY'
import hashlib
import os
from pathlib import Path

directory = Path(os.environ["RAMIZ_PACKAGE_CHECKSUM_DIR"])
archives = sorted(directory.glob("ramiz_*.tar.gz"))
if len(archives) != 4:
    raise SystemExit("checksums requires exactly four ramiz_*.tar.gz archives")
names = [item.name for item in archives]
versions = {name.removeprefix("ramiz_").rsplit("_", 2)[0] for name in names}
if len(versions) != 1:
    raise SystemExit("checksum directory contains mixed archive versions")
version = versions.pop()
expected = {f"ramiz_{version}_{platform}_{arch}.tar.gz" for platform in ("darwin", "linux") for arch in ("amd64", "arm64")}
if set(names) != expected:
    raise SystemExit("checksum directory does not contain the four supported archives")
(directory / "checksums.txt").write_text("".join(f"{hashlib.sha256(path.read_bytes()).hexdigest()}  {path.name}\n" for path in archives))
PY
        ;;
    *)
        usage
        ;;
esac
