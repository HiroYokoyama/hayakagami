//! Exclusion patterns.
//!
//! `*.tmp` and `node_modules` match a name at any depth. A pattern containing
//! `/` is anchored to the mirrored folder (`docs/draft`). A trailing `/` limits
//! the pattern to folders (`build/`). Matching ignores case.

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};

#[derive(Clone)]
pub struct Excludes {
    any: GlobSet,
    dirs: GlobSet,
    empty: bool,
}

impl Excludes {
    pub fn new(patterns: &[String]) -> Result<Self, String> {
        let mut any = GlobSetBuilder::new();
        let mut dirs = GlobSetBuilder::new();
        let mut empty = true;
        for raw in patterns {
            let trimmed = raw.trim().replace('\\', "/");
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            empty = false;
            let dir_only = trimmed.ends_with('/');
            let body = trimmed.trim_end_matches('/');
            let glob = if body.contains('/') {
                body.trim_start_matches('/').to_string()
            } else {
                format!("**/{body}")
            };
            let built = GlobBuilder::new(&glob)
                .case_insensitive(true)
                .literal_separator(true)
                .build()
                .map_err(|e| {
                    crate::trf!(
                        "Invalid exclusion pattern `{raw}`: {e}",
                        "除外パターン `{raw}` が不正です: {e}"
                    )
                })?;
            if dir_only {
                dirs.add(built);
            } else {
                any.add(built);
            }
        }
        let finish = |b: GlobSetBuilder| {
            b.build()
                .map_err(|e| crate::trf!("Exclusion pattern error: {e}", "除外パターンエラー: {e}"))
        };
        Ok(Self {
            any: finish(any)?,
            dirs: finish(dirs)?,
            empty,
        })
    }

    pub fn is_empty(&self) -> bool {
        self.empty
    }

    /// `rel` uses `/` as its separator.
    pub fn matches(&self, rel: &str, is_dir: bool) -> bool {
        self.any.is_match(rel) || (is_dir && self.dirs.is_match(rel))
    }
}
