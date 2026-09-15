# Security Policy

Security fixes are released on the latest stable Ramiz version. Before
reporting an issue already fixed in a newer release, upgrade through the
installer that owns the current executable.

Please report vulnerabilities privately through GitHub's security-advisory
form for `agensfield/ramiz`. Do not include repository contents, credentials,
or private worktree data in a public issue.

## Release integrity

Each release publishes SHA-256 checksums, Minisign signatures, and GitHub
build-provenance attestations. The pinned Minisign public key is in
`release/minisign.pub`. Verify the checksum manifest and an archive with:

```sh
minisign -Vm checksums.txt -p release/minisign.pub -x checksums.txt.minisig
shasum -a 256 -c checksums.txt
gh attestation verify <archive> --repo agensfield/ramiz
```

Release tags and assets are immutable. A defective release is superseded by a
new version rather than replaced in place.
