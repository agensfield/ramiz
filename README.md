# Ramiz

> Ramiz is the dayı in the İstanbul çarşı who gets you ridiculously cheap Git
> worktrees.

Ramiz creates ordinary Git worktrees using filesystem copy-on-write. It uses
APFS `clonefile(2)` on macOS and `FICLONE` reflinks on Linux, then leaves the
worktree entirely to Git.

```console
$ ramiz add ../feature
$ git ramiz add --inherit -b feature ../feature
```

Clean creation is the default. Ramiz retains only donor files proven identical
to the target Git objects and lets Git materialize transformed or changed
paths. `--inherit` instead carries the selected donor's staged, unstaged,
untracked, and ignored state when the target is the donor's exact `HEAD`.

## Install

Homebrew on macOS or Linux:

```sh
brew install agensfield/tap/ramiz
```

Prebuilt binaries through Cargo Binstall:

```sh
cargo binstall ramiz
```

Build from source with Rust 1.85 or newer:

```sh
cargo install ramiz --locked
```

Every supported installation provides `ramiz` and `git-ramiz`; Git discovers
the latter automatically as `git ramiz`. Git 2.42 or newer is required.

Signed and checksummed standalone archives for macOS and Linux on arm64 and
x86_64 are attached to each GitHub release. Ramiz does not provide a
`curl | sh` installer.

After extracting an archive into a directory on `PATH`, adopt that verified
standalone installation once so Ramiz can update both invocation binaries as
one unit:

```sh
ramiz update --adopt
```

Ramiz refuses adoption when it detects another package manager. Homebrew and
Cargo installations remain owned by those installers.

## Usage

```text
ramiz add [-b <new-branch> | --detach] <path> [<start-point>]
ramiz doctor [path]
ramiz update [--check | --adopt]
```

Useful creation controls:

- `--require-cow` refuses ordinary-checkout fallback.
- `--inherit` carries the donor's complete lived-in state.
- `--from <path>` selects another registered worktree in the same repository.
- `--allow-copy` explicitly permits a physical copy for inheritance.
- `--allow-donor-links` accepts inherited links that resolve into the donor.
- `--json` emits one stable `ramiz.cli/v1` envelope.

Ramiz creates worktrees. Use native Git for everything afterward:

```sh
git worktree list
git worktree move ...
git worktree remove ...
git worktree repair
git worktree prune
```

## Safety model

Ramiz validates branch intent, destination ownership, donor identity, Git index
features, filesystem capability, and target equality before materialization.
Before `post-checkout` begins, failures remove only worktree and branch state
that Ramiz can prove it owns. A failing hook preserves the completed worktree
and returns a structured error.

Complete inheritance detects ordinary donor changes around creation, but does
not claim a transactional snapshot against hostile or undetectable concurrent
writes. Keep the donor idle while inheritance runs.

## License and prior art

Ramiz is MIT-licensed. Its copy-on-write worktree design was informed by Can
Bölük's MIT-licensed work in [Oh My Pi](https://github.com/can1357/oh-my-pi),
particularly feature commit `ff91512b1d2927dd4cc095240ecf63749a32fca8`.
See [NOTICE](NOTICE).
