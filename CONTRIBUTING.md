# Contributing to Hayakagami

Thank you for helping. Hayakagami deletes and overwrites files on people's disks, so changes are held to one rule above the others: **when in doubt, do the slower, safer thing.**

Issues and pull requests can be written in English or Japanese.

## Reporting

- **Bugs and ideas:** open an [issue](https://github.com/HiroYokoyama/hayakagami/issues/new/choose).
- **A bug that can lose or damage data, or a security problem:** see [SECURITY.md](SECURITY.md). Do not put it in a public issue if others could be harmed before it is fixed.

## Setting up

You need Rust 1.95 or later. Linux also needs `libgtk-3-dev`, `libxkbcommon-dev` and `libgl1-mesa-dev`.

```bash
cargo build
```

## Checks

Pull requests must pass these on Windows, macOS and Linux. Continuous integration runs them on all three.

```bash
cargo fmt --check
```

```bash
cargo clippy --all-targets -- -D warnings
```

```bash
cargo test
```

Code behind `#[cfg(windows)]` or `#[cfg(unix)]` is not compiled on the other system, so a change that passes locally can still fail elsewhere.

### Tests that need administrator rights

Reading the NTFS change journal usually needs administrator rights, so those tests skip themselves without them. Setting `HAYAKAGAMI_REQUIRE_USN=1`, as continuous integration does, makes a skip a failure. The same variable requires symbolic links to work on Windows.

Run them on your own machine with care, as an administrator, or rely on continuous integration.

## Rules for changes

### Protecting data

- Tests work only inside temporary folders. A test must never touch a real path, a drive letter or a user's settings. Set `HAYAKAGAMI_HOME` so that settings and saved state stay in a temporary folder too.
- `plan::scan` never changes anything on disk. All changes belong in `exec::execute`.
- The source is only ever read.
- Never remove folders recursively. A folder is removed only once it is empty, so that a mistake cannot take a whole tree with it.
- Never follow links, and never write through one.
- A new way of skipping work, such as a saved state, must fall back to reading the disk whenever it cannot be trusted. A wrong full scan costs time; a wrong shortcut costs data.
- A change to what gets deleted or overwritten needs a test showing that the safety checks still stop it.

### Style

- Comments explain why, not what.
- User-facing text goes through `tr` or `trf!`, in both English and Japanese.
- Keep the English and Japanese READMEs in step.
- Add a line to `CHANGELOG.md` for changes people will notice.
- The saved state's format has a version in its first bytes. Change it whenever the format changes, so old files are rejected and lead to a full check.

### Pull requests

Keep each one to a single purpose, and say how you tested it, especially on any system you could not run.

## Benchmarks

The `Benchmark` workflow generates 100,000 files on GitHub's runners. It runs only on request, so start it from the Actions tab when a change could affect speed. Please do not generate large test data on machines that are doing real work.

## Releases

The maintainer publishes releases by pushing a `vX.Y.Z` tag, after which continuous integration builds the archives. Bump the version in `Cargo.toml` first.

## License

By contributing, you agree that your work is released under the [GNU Lesser General Public License, version 3 or later](LICENSE).
