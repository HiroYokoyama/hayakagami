//! Saved patterns: named sets of folder pairs and exclusions.

use crate::model::DiskIdentity;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct FolderPair {
    pub source: String,
    pub dest: String,
    /// Exclusions for this pair, added to the pattern's list.
    #[serde(default)]
    pub excludes: Vec<String>,
    /// When set, a run stops unless the destination is on this disk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dest_disk: Option<DiskIdentity>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Pattern {
    pub name: String,
    #[serde(default)]
    pub pairs: Vec<FolderPair>,
    #[serde(default)]
    pub excludes: Vec<String>,
    #[serde(default = "default_full_check_days")]
    pub full_check_days: u32,
    /// Run instead of previewing when this pattern's file is opened.
    #[serde(default)]
    pub run_on_open: bool,
    /// Read the NTFS change journal to avoid listing unchanged source folders.
    #[serde(default)]
    pub use_change_journal: bool,
    /// Recreate links in the destination, with their targets unchanged.
    #[serde(default)]
    pub copy_links: bool,
}

/// Extension of a pattern saved as its own file.
pub const PATTERN_EXTENSION: &str = "hkgm";
const PATTERN_FORMAT: &str = "hayakagami-pattern";
const PATTERN_VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
struct PatternFile {
    format: String,
    version: u32,
    #[serde(flatten)]
    pattern: Pattern,
}

fn default_full_check_days() -> u32 {
    30
}

impl Pattern {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            pairs: vec![FolderPair::default()],
            excludes: Vec::new(),
            full_check_days: default_full_check_days(),
            run_on_open: false,
            use_change_journal: false,
            copy_links: false,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_language")]
    pub language: String,
    #[serde(default)]
    pub patterns: Vec<Pattern>,
}

fn default_language() -> String {
    "en".to_string()
}

impl Default for Config {
    fn default() -> Self {
        Self {
            language: default_language(),
            patterns: Vec::new(),
        }
    }
}

fn base_dir(windows_var: &str, mac_sub: &str, xdg_var: &str, xdg_default: &str) -> PathBuf {
    let home = || PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| ".".into()));
    if cfg!(windows) {
        std::env::var_os(windows_var).map_or_else(|| PathBuf::from("."), PathBuf::from)
    } else if cfg!(target_os = "macos") {
        home().join("Library").join(mac_sub)
    } else {
        std::env::var_os(xdg_var).map_or_else(|| home().join(xdg_default), PathBuf::from)
    }
}

/// `HAYAKAGAMI_HOME` overrides both directories, keeping tests away from real settings.
pub fn config_dir() -> PathBuf {
    if let Some(home) = std::env::var_os("HAYAKAGAMI_HOME") {
        return PathBuf::from(home);
    }
    base_dir(
        "APPDATA",
        "Application Support",
        "XDG_CONFIG_HOME",
        ".config",
    )
    .join("Hayakagami")
}

pub fn cache_dir() -> PathBuf {
    if let Some(home) = std::env::var_os("HAYAKAGAMI_HOME") {
        return PathBuf::from(home).join("cache");
    }
    base_dir("LOCALAPPDATA", "Caches", "XDG_CACHE_HOME", ".cache").join("Hayakagami")
}

pub fn config_path() -> PathBuf {
    config_dir().join("patterns.json")
}

pub fn load(path: &Path) -> Result<Config, String> {
    match fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).map_err(|e| {
            crate::trf!(
                "Cannot read {}: {e}",
                "{} を読み込めません: {e}",
                path.display()
            )
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(e) => Err(crate::trf!(
            "Cannot read {}: {e}",
            "{} を読み込めません: {e}",
            path.display()
        )),
    }
}

/// Writes through a temporary file, so a crash cannot truncate saved patterns.
pub fn save(path: &Path, config: &Config) -> Result<(), String> {
    let run = || -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let text = serde_json::to_string_pretty(config)?;
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, text)?;
        fs::rename(&tmp, path)
    };
    run().map_err(|e| {
        crate::trf!(
            "Cannot save {}: {e}",
            "{} に保存できません: {e}",
            path.display()
        )
    })
}

/// Records the language without touching anything else in the settings.
pub fn save_language(path: &Path, code: &str) -> Result<(), String> {
    let mut config = load(path)?;
    config.language = code.to_owned();
    save(path, &config)
}

pub fn load_pattern_file(path: &Path) -> Result<Pattern, String> {
    let text = fs::read_to_string(path).map_err(|e| {
        crate::trf!(
            "Cannot read {}: {e}",
            "{} を読み込めません: {e}",
            path.display()
        )
    })?;
    let file: PatternFile = serde_json::from_str(&text).map_err(|e| {
        crate::trf!(
            "{} is not a pattern file: {e}",
            "{} はパターンファイルではありません: {e}",
            path.display()
        )
    })?;
    if file.format != PATTERN_FORMAT {
        return Err(crate::trf!(
            "{} is not a pattern file",
            "{} はパターンファイルではありません",
            path.display()
        ));
    }
    if file.version > PATTERN_VERSION {
        return Err(crate::trf!(
            "{} needs a newer version of Hayakagami",
            "{} を開くには新しいバージョンのHayakagamiが必要です",
            path.display()
        ));
    }
    Ok(file.pattern)
}

pub fn save_pattern_file(path: &Path, pattern: &Pattern) -> Result<(), String> {
    let file = PatternFile {
        format: PATTERN_FORMAT.to_owned(),
        version: PATTERN_VERSION,
        pattern: pattern.clone(),
    };
    let run = || -> std::io::Result<()> {
        let text = serde_json::to_string_pretty(&file)?;
        let tmp = path.with_extension("hkgm.tmp");
        fs::write(&tmp, text)?;
        fs::rename(&tmp, path)
    };
    run().map_err(|e| {
        crate::trf!(
            "Cannot save {}: {e}",
            "{} に保存できません: {e}",
            path.display()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_pattern_has_no_exclusions_and_does_not_auto_run() {
        let pattern = Pattern::new("p");
        assert!(pattern.excludes.is_empty());
        assert!(pattern.pairs[0].excludes.is_empty());
        assert!(!pattern.run_on_open);
    }

    #[test]
    fn pattern_file_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.hkgm");
        let mut pattern = Pattern::new("日本語");
        pattern.excludes.push("*.tmp".into());
        pattern.pairs[0].source = r"D:\\from".into();
        save_pattern_file(&path, &pattern).unwrap();
        let back = load_pattern_file(&path).unwrap();
        assert_eq!(back.name, "日本語");
        assert_eq!(back.excludes, ["*.tmp"]);
        assert_eq!(back.pairs[0].source, pattern.pairs[0].source);
    }

    #[test]
    fn other_json_is_not_a_pattern_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.hkgm");
        fs::write(&path, r#"{"name":"x"}"#).unwrap();
        assert!(load_pattern_file(&path).is_err());
        fs::write(&path, r#"{"format":"other","version":1,"name":"x"}"#).unwrap();
        assert!(load_pattern_file(&path).is_err());
        fs::write(
            &path,
            r#"{"format":"hayakagami-pattern","version":99,"name":"x"}"#,
        )
        .unwrap();
        assert!(load_pattern_file(&path).is_err());
    }
}
