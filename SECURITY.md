# Security policy

## Supported versions

Fixes go into the latest release only. Please update before reporting.

## Reporting a problem

Please report privately, through [GitHub's vulnerability reporting](https://github.com/HiroYokoyama/hikae/security/advisories/new). English and Japanese are both fine.

Report privately if you find:

- a way for Hikae to delete or overwrite data it should not, including through a crafted `.hikae` file or a link
- a way to misuse the elevated change journal helper, or the administrator rights it holds
- a way for a run to report success while a backup is damaged or incomplete

Please include the version, your operating system, the steps to reproduce it and what you expected. This is a personal project, so there is no guaranteed response time.

## What to expect from the design

- The source is only read. Administrator rights are used only by a helper that reads the change journal, and it writes no files.
- A mirror keeps no history. A file damaged or deleted in the source is damaged or deleted in the destination at the next run, unless a safety check stops it. Keep a versioned backup of anything important.
- A `.hikae` file from someone else decides which folders are mirrored, and so which files are deleted. Read it before opening it.
- The programs are not code-signed.
