//! Display options, kept in `$XDG_CONFIG_HOME/rexroster/config.toml`
//! (`~/.config/rexroster/config.toml` when that is unset).
//!
//! ```toml
//! border = "rounded"   # or "none" (the default)
//! view = "expanded"    # or "compact" (the default)
//! ```
//!
//! A missing file or key means the default, and unknown keys are ignored.
//! The options panel saves back only the keys it changed, keeping the rest
//! of the file, comments included.

use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use toml_edit::{DocumentMut, value};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum)]
pub enum Border {
    #[default]
    None,
    Rounded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum)]
pub enum View {
    #[default]
    Compact,
    Expanded,
}

/// Lowercase names, as written in the file and on the command line.
trait Named: Sized + Copy + PartialEq + 'static {
    const ALL: &'static [Self];
    fn name(self) -> &'static str;

    fn parse(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|v| v.name() == s)
    }

    /// The next (or previous) value, wrapping round.
    fn step(self, by: isize) -> Self {
        let all = Self::ALL;
        let i = all.iter().position(|v| *v == self).unwrap_or(0);
        let len = isize::try_from(all.len()).unwrap_or(1);
        let j = (isize::try_from(i).unwrap_or(0) + by).rem_euclid(len);
        all[usize::try_from(j).unwrap_or(0)]
    }
}

impl Named for Border {
    const ALL: &'static [Self] = &[Border::None, Border::Rounded];
    fn name(self) -> &'static str {
        match self {
            Border::None => "none",
            Border::Rounded => "rounded",
        }
    }
}

impl Named for View {
    const ALL: &'static [Self] = &[View::Compact, View::Expanded];
    fn name(self) -> &'static str {
        match self {
            View::Compact => "compact",
            View::Expanded => "expanded",
        }
    }
}

impl fmt::Display for Border {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl fmt::Display for View {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(self.name())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Options {
    pub border: Border,
    /// The view the roster starts in.
    pub view: View,
}

/// One option, as listed in the options panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Border,
    View,
}

impl Key {
    pub const ALL: [Key; 2] = [Key::Border, Key::View];

    pub fn name(self) -> &'static str {
        match self {
            Key::Border => "border",
            Key::View => "view",
        }
    }
}

impl Options {
    /// Flags win over the file.
    pub fn with_flags(self, border: Option<Border>, view: Option<View>) -> Self {
        Self {
            border: border.unwrap_or(self.border),
            view: view.unwrap_or(self.view),
        }
    }

    pub fn value(&self, key: Key) -> &'static str {
        match key {
            Key::Border => self.border.name(),
            Key::View => self.view.name(),
        }
    }

    /// Move `key` to its next (or previous) value.
    pub fn cycle(&mut self, key: Key, by: isize) {
        match key {
            Key::Border => self.border = self.border.step(by),
            Key::View => self.view = self.view.step(by),
        }
    }
}

/// Parse the file's text. Missing keys are defaults; unknown keys are ignored.
pub fn parse(text: &str) -> Result<Options> {
    let doc: DocumentMut = text.parse().map_err(|e: toml_edit::TomlError| {
        anyhow::anyhow!("{}", e.message().lines().next().unwrap_or("invalid TOML"))
    })?;
    let mut opts = Options::default();
    if let Some(item) = doc.get("border") {
        let s = item.as_str().unwrap_or_default();
        opts.border = Border::parse(s)
            .with_context(|| format!("border must be \"none\" or \"rounded\", not {item}"))?;
    }
    if let Some(item) = doc.get("view") {
        let s = item.as_str().unwrap_or_default();
        opts.view = View::parse(s)
            .with_context(|| format!("view must be \"compact\" or \"expanded\", not {item}"))?;
    }
    Ok(opts)
}

/// The config file's path, if a config directory can be found.
pub fn path() -> Option<PathBuf> {
    path_from(
        std::env::var_os("XDG_CONFIG_HOME"),
        std::env::var_os("HOME"),
    )
}

fn path_from(xdg: Option<std::ffi::OsString>, home: Option<std::ffi::OsString>) -> Option<PathBuf> {
    let base = xdg
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| home.map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("rexroster").join("config.toml"))
}

/// Load the options. A missing file gives the defaults with no error; an
/// unreadable or malformed one gives the defaults and a one-line error.
pub fn load(path: Option<&Path>) -> (Options, Option<String>) {
    let Some(path) = path else {
        return (Options::default(), None);
    };
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (Options::default(), None),
        Err(e) => return (Options::default(), Some(format!("config: {e}"))),
    };
    match parse(&text) {
        Ok(opts) => (opts, None),
        Err(e) => (
            Options::default(),
            Some(format!("config {}: {e:#}", path.display())),
        ),
    }
}

/// Write `keys` from `opts` into the file at `path`, keeping everything else
/// in it. Creates the directory if needed and replaces the file atomically.
pub fn save(path: &Path, opts: &Options, keys: &[Key]) -> Result<()> {
    if keys.is_empty() {
        return Ok(());
    }
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let Ok(mut doc) = text.parse::<DocumentMut>() else {
        bail!("{} is not valid TOML, so it was not saved", path.display());
    };
    for key in keys {
        let mut new = value(opts.value(*key));
        // Keep any comment or spacing around the old value.
        if let (Some(old), Some(v)) = (
            doc.get(key.name()).and_then(|i| i.as_value()),
            new.as_value_mut(),
        ) {
            *v.decor_mut() = old.decor().clone();
        }
        doc[key.name()] = new;
    }
    let dir = path.parent().context("config path has no directory")?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let tmp = dir.join(format!(".config.toml.{}.tmp", std::process::id()));
    std::fs::write(&tmp, doc.to_string()).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("rexroster-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn parses_full_partial_and_empty_files() {
        assert_eq!(parse("").unwrap(), Options::default());
        assert_eq!(
            parse("border = \"rounded\"\n").unwrap(),
            Options {
                border: Border::Rounded,
                view: View::Compact
            }
        );
        assert_eq!(
            parse("border = \"none\"\nview = \"expanded\"\n").unwrap(),
            Options {
                border: Border::None,
                view: View::Expanded
            }
        );
    }

    #[test]
    fn ignores_unknown_keys() {
        let opts = parse("colour = \"teal\"\nview = \"expanded\"\n[extra]\nx = 1\n").unwrap();
        assert_eq!(opts.view, View::Expanded);
    }

    #[test]
    fn malformed_files_are_errors() {
        assert!(parse("border = ").is_err());
        assert!(parse("border = \"square\"").is_err());
        assert!(parse("view = 3").is_err());
    }

    #[test]
    fn load_falls_back_to_defaults_with_one_line_errors() {
        let dir = temp_dir("load");
        let file = dir.join("config.toml");
        assert_eq!(load(Some(&file)), (Options::default(), None));
        assert_eq!(load(None), (Options::default(), None));

        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&file, "border = [oops\n").unwrap();
        let (opts, err) = load(Some(&file));
        assert_eq!(opts, Options::default());
        let err = err.unwrap();
        assert!(!err.contains('\n'), "{err}");
        assert!(err.starts_with("config "), "{err}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn flags_win_over_the_file() {
        let file = Options {
            border: Border::Rounded,
            view: View::Expanded,
        };
        assert_eq!(file.with_flags(None, None), file);
        assert_eq!(
            file.with_flags(Some(Border::None), None),
            Options {
                border: Border::None,
                view: View::Expanded
            }
        );
        assert_eq!(
            file.with_flags(None, Some(View::Compact)).view,
            View::Compact
        );
    }

    #[test]
    fn cycles_wrap_both_ways() {
        let mut o = Options::default();
        o.cycle(Key::Border, 1);
        assert_eq!(o.border, Border::Rounded);
        o.cycle(Key::Border, 1);
        assert_eq!(o.border, Border::None);
        o.cycle(Key::View, -1);
        assert_eq!(o.view, View::Expanded);
    }

    #[test]
    fn save_round_trips_and_keeps_other_content() {
        let dir = temp_dir("save");
        let file = dir.join("nested").join("config.toml");
        let opts = Options {
            border: Border::Rounded,
            view: View::Expanded,
        };
        // Creates the directory; writes only the keys asked for.
        save(&file, &opts, &[Key::Border]).unwrap();
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "border = \"rounded\"\n"
        );
        assert_eq!(
            parse(&std::fs::read_to_string(&file).unwrap())
                .unwrap()
                .view,
            View::Compact
        );

        let original = "# my roster\ncolour = \"teal\" # unknown, kept\nborder = \"none\"\n";
        std::fs::write(&file, original).unwrap();
        save(&file, &opts, &[Key::Border, Key::View]).unwrap();
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(
            text.starts_with("# my roster\ncolour = \"teal\" # unknown, kept\n"),
            "{text}"
        );
        assert_eq!(parse(&text).unwrap(), opts);
        // No temp files left behind.
        let names: Vec<_> = std::fs::read_dir(file.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["config.toml"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn save_refuses_to_overwrite_a_malformed_file() {
        let dir = temp_dir("bad");
        let file = dir.join("config.toml");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&file, "border = [oops").unwrap();
        assert!(save(&file, &Options::default(), &[Key::Border]).is_err());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "border = [oops");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn path_prefers_xdg_config_home() {
        let p = |x: Option<&str>, h: Option<&str>| path_from(x.map(Into::into), h.map(Into::into));
        assert_eq!(
            p(Some("/x"), Some("/home/u")),
            Some(PathBuf::from("/x/rexroster/config.toml"))
        );
        // A relative XDG_CONFIG_HOME is ignored, as the spec says.
        assert_eq!(
            p(Some("rel"), Some("/home/u")),
            Some(PathBuf::from("/home/u/.config/rexroster/config.toml"))
        );
        assert_eq!(p(None, None), None);
    }

    #[test]
    fn save_keeps_a_comment_on_the_changed_key() {
        // Review finding: the trailing comment was dropped.
        let dir = temp_dir("comment");
        let file = dir.join("config.toml");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&file, "border = \"none\"  # keep it clean\n").unwrap();
        let opts = Options {
            border: Border::Rounded,
            ..Options::default()
        };
        save(&file, &opts, &[Key::Border]).unwrap();
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "border = \"rounded\"  # keep it clean\n"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
