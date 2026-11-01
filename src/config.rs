//! Options, kept in `$XDG_CONFIG_HOME/rexroster/config.toml`
//! (`~/.config/rexroster/config.toml` when that is unset).
//!
//! ```toml
//! border = "rounded"   # or "none" (the default)
//! view = "expanded"    # or "compact" (the default)
//! watch = false        # or true (the default): open and spawn start the watcher
//! notify = "cases"     # or "none" (the default): what the watcher does besides floating
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

/// What the watcher does besides floating a blocked worker into view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Notify {
    #[default]
    None,
    /// Open a case with the `cases` CLI, and withdraw it when the worker moves on.
    Cases,
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

impl Named for Notify {
    const ALL: &'static [Self] = &[Notify::None, Notify::Cases];
    fn name(self) -> &'static str {
        match self {
            Notify::None => "none",
            Notify::Cases => "cases",
        }
    }
}

impl Notify {
    pub fn as_str(self) -> &'static str {
        self.name()
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Options {
    pub border: Border,
    /// The view the roster starts in.
    pub view: View,
    /// Whether `open` and `spawn` start the watcher.
    pub watch: bool,
    pub notify: Notify,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            border: Border::default(),
            view: View::default(),
            watch: true,
            notify: Notify::default(),
        }
    }
}

/// One option, as listed in the options panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Border,
    View,
    Watch,
    Notify,
}

impl Key {
    pub const ALL: [Key; 4] = [Key::Border, Key::View, Key::Watch, Key::Notify];

    pub fn name(self) -> &'static str {
        match self {
            Key::Border => "border",
            Key::View => "view",
            Key::Watch => "watch",
            Key::Notify => "notify",
        }
    }
}

impl Options {
    /// Flags win over the file.
    pub fn with_flags(self, border: Option<Border>, view: Option<View>) -> Self {
        Self {
            border: border.unwrap_or(self.border),
            view: view.unwrap_or(self.view),
            ..self
        }
    }

    pub fn value(&self, key: Key) -> &'static str {
        match key {
            Key::Border => self.border.name(),
            Key::View => self.view.name(),
            Key::Watch => bool_name(self.watch),
            Key::Notify => self.notify.name(),
        }
    }

    /// `key`'s value as written in the file: `watch` is a boolean, the rest strings.
    fn toml_value(&self, key: Key) -> toml_edit::Item {
        match key {
            Key::Watch => value(self.watch),
            _ => value(self.value(key)),
        }
    }

    /// Move `key` to its next (or previous) value.
    pub fn cycle(&mut self, key: Key, by: isize) {
        match key {
            Key::Border => self.border = self.border.step(by),
            Key::View => self.view = self.view.step(by),
            Key::Watch => self.watch = !self.watch,
            Key::Notify => self.notify = self.notify.step(by),
        }
    }
}

fn bool_name(b: bool) -> &'static str {
    if b { "true" } else { "false" }
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
    if let Some(item) = doc.get("watch") {
        opts.watch = item
            .as_bool()
            .with_context(|| format!("watch must be true or false, not {item}"))?;
    }
    if let Some(item) = doc.get("notify") {
        let s = item.as_str().unwrap_or_default();
        opts.notify = Notify::parse(s)
            .with_context(|| format!("notify must be \"none\" or \"cases\", not {item}"))?;
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

/// Load the options, or an error if the file can't be read or parsed. A
/// missing file gives the defaults. For the watcher, where falling back to
/// the defaults could turn on what the file turned off.
pub fn load_strict(path: Option<&Path>) -> Result<Options> {
    match load(path) {
        (opts, None) => Ok(opts),
        (_, Some(err)) => bail!("{err}"),
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
        let mut new = opts.toml_value(*key);
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
                view: View::Compact,
                ..Options::default()
            }
        );
        assert_eq!(
            parse("border = \"none\"\nview = \"expanded\"\n").unwrap(),
            Options {
                border: Border::None,
                view: View::Expanded,
                ..Options::default()
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
            ..Options::default()
        };
        assert_eq!(file.with_flags(None, None), file);
        assert_eq!(
            file.with_flags(Some(Border::None), None),
            Options {
                border: Border::None,
                view: View::Expanded,
                ..Options::default()
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
            ..Options::default()
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

    #[test]
    fn parses_watch_and_notify() {
        let d = parse("").unwrap();
        assert!(d.watch);
        assert_eq!(d.notify, Notify::None);
        let o = parse("watch = false\nnotify = \"cases\"\n").unwrap();
        assert!(!o.watch);
        assert_eq!(o.notify, Notify::Cases);
        assert!(parse("watch = \"no\"").is_err());
        assert!(parse("notify = \"email\"").is_err());
        // Flags change only the display options.
        assert_eq!(
            o.with_flags(Some(Border::Rounded), None).notify,
            Notify::Cases
        );
    }

    #[test]
    fn cycles_and_saves_watch_as_a_boolean() {
        let dir = temp_dir("watch");
        let file = dir.join("config.toml");
        let mut o = Options::default();
        o.cycle(Key::Watch, 1);
        assert!(!o.watch);
        assert_eq!(o.value(Key::Watch), "false");
        o.cycle(Key::Notify, -1);
        assert_eq!(o.notify, Notify::Cases);
        save(&file, &o, &[Key::Watch, Key::Notify]).unwrap();
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "watch = false\nnotify = \"cases\"\n"
        );
        assert_eq!(parse(&std::fs::read_to_string(&file).unwrap()).unwrap(), o);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn load_strict_refuses_a_bad_file() {
        // Review finding: `watch = false` beside a typo fell back to watch = true.
        let dir = temp_dir("strict");
        let file = dir.join("config.toml");
        assert!(load_strict(Some(&file)).unwrap().watch);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&file, "watch = false\nview = \"expandd\"\n").unwrap();
        assert!(load_strict(Some(&file)).is_err());
        std::fs::write(&file, "watch = false\n").unwrap();
        assert!(!load_strict(Some(&file)).unwrap().watch);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
