# Changelog

## 0.3.1

### Added

- An overall progress bar in the desktop app, across all folder pairs of a run, with the time so far.
- Metadata updates appear as their own step with a count, instead of "Deleting 0 / 0".

### Changed

- Metadata is updated by several threads on SSD and network destinations. Hard disks keep one thread.

## 0.3.0

The first run after updating is a full check, because the saved state has a new format.

### Changed

- Much less writing to the disk holding the saved state. It is now a log: a run adds only what changed, instead of rewriting the whole file. With the change journal, a run that finds nothing adds about 100 bytes, where it used to write about 90 MB per million files.

## 0.2.1

- Added the license (LGPL-3.0-or-later).
- Release archives now include the licenses of Hayakagami and of the crates built into it, and the Japanese README.

## 0.2.0

The first run after updating is a full check, because the saved state has a new format.

### Added

- Optional use of the NTFS change journal to skip unchanged source folders (Windows, off by default). The desktop app asks for permission once per window, and a small helper uses it only to read the journal. A pattern file with this turned on asks when it is opened.
- Optional copying of links as links, with their targets unchanged (off by default).
- Metadata updates without copying contents: read-only, hidden and system attributes, creation times, permission bits and folder modification times.
- A safety check that stops when a tenth or more of the destination's files would be overwritten (when it holds 10 files or more).
- Benchmark results in the READMEs.

### Changed

- Copies are forced onto the disk before they replace an existing file, so a power cut cannot leave a damaged file that looks complete. Copying many small files is slower as a result.
- The icon's folders have usual proportions.
- The benchmark workflow runs only on request.

## 0.1.0

First release.

- Folder-based mirroring for Windows, macOS and Linux, with a desktop app and a command line.
- Named patterns, which can also be saved as `.hkgm` files and opened with a double-click on Windows.
- A saved state of the destination, so that unchanged runs do not read it.
- Safety checks against empty sources, unfamiliar destinations and mass deletion.
- English and Japanese.
