use std::path::{Path, PathBuf};

use ratatui::crossterm::event::{KeyCode, KeyModifiers};
use serde::Deserialize;

pub const LOCAL_FILE: &str = ".gtui-terms.toml";

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub baud: Option<u32>,
    pub port: Option<String>,
    pub timestamps: Option<bool>,
    /// Idle gap in ms that closes an RX packet in packet view (0 = one packet per read).
    pub packet_gap_ms: Option<u64>,
    pub log: Option<PathBuf>,
    pub elf: Option<PathBuf>,
    /// Line ending appended to sent text: crlf (default), lf, cr, none.
    pub eol: Option<String>,
    pub data_bits: Option<u64>,
    pub parity: Option<String>,
    pub stop_bits: Option<u64>,
    pub flow: Option<String>,
    /// Skip the reset pulse sent right after opening the port.
    pub no_reset: Option<bool>,
    pub macros: Vec<MacroDef>,
    pub hooks: Vec<HookDef>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HookDef {
    pub name: String,
    /// Shell command; the port is released before it starts and reopened after.
    pub command: String,
    /// Optional regex: matching any received line fires the hook once.
    #[serde(default)]
    pub trigger: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Hook {
    pub name: String,
    pub command: String,
    pub trigger: Option<regex::Regex>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MacroDef {
    pub name: String,
    pub text: String,
    /// `f9`..`f12` or `alt+<char>`.
    pub key: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Macro {
    pub name: String,
    pub text: String,
    pub key: KeyCode,
    pub mods: KeyModifiers,
}

#[derive(Debug)]
pub struct Loaded {
    pub config: Config,
    pub macros: Vec<Macro>,
    pub hooks: Vec<Hook>,
    pub warnings: Vec<String>,
}

impl Config {
    fn merge(&mut self, other: Config) {
        self.baud = other.baud.or(self.baud);
        self.port = other.port.or_else(|| self.port.take());
        self.timestamps = other.timestamps.or(self.timestamps);
        self.packet_gap_ms = other.packet_gap_ms.or(self.packet_gap_ms);
        self.log = other.log.or_else(|| self.log.take());
        self.elf = other.elf.or_else(|| self.elf.take());
        self.eol = other.eol.or_else(|| self.eol.take());
        self.data_bits = other.data_bits.or(self.data_bits);
        self.parity = other.parity.or_else(|| self.parity.take());
        self.stop_bits = other.stop_bits.or(self.stop_bits);
        self.flow = other.flow.or_else(|| self.flow.take());
        self.no_reset = other.no_reset.or(self.no_reset);
        for entry in other.macros {
            self.macros.retain(|existing| existing.name != entry.name);
            self.macros.push(entry);
        }
        self.hooks.extend(other.hooks);
    }
}

/// Loads global config then `--config`/local file, later values winning.
pub fn load(explicit: Option<&Path>) -> Result<Loaded, String> {
    let mut config = Config::default();
    let mut warnings = Vec::new();

    if let Some(path) = global_path() {
        merge_file(&mut config, &path, false, &mut warnings)?;
    }
    match explicit {
        Some(path) => merge_file(&mut config, path, true, &mut warnings)?,
        None => {
            let local = PathBuf::from(LOCAL_FILE);
            merge_file(&mut config, &local, false, &mut warnings)?;
        }
    }

    let macros = compile_macros(&config.macros, &mut warnings);
    let hooks = compile_hooks(&config.hooks, &mut warnings);
    Ok(Loaded {
        config,
        macros,
        hooks,
        warnings,
    })
}

fn compile_hooks(defs: &[HookDef], warnings: &mut Vec<String>) -> Vec<Hook> {
    let mut hooks = Vec::new();
    for def in defs {
        let trigger = match def.trigger.as_deref() {
            None => None,
            Some(pattern) => match regex::Regex::new(pattern) {
                Ok(regex) => Some(regex),
                Err(err) => {
                    warnings.push(format!("hook {}: bad trigger {pattern:?}: {err}", def.name));
                    None
                }
            },
        };
        hooks.push(Hook {
            name: def.name.clone(),
            command: def.command.clone(),
            trigger,
        });
    }
    hooks
}

fn merge_file(
    config: &mut Config,
    path: &Path,
    required: bool,
    warnings: &mut Vec<String>,
) -> Result<(), String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound && !required => return Ok(()),
        Err(err) => return Err(format!("{path:?}: {err}")),
    };
    match toml::from_str::<Config>(&text) {
        Ok(parsed) => config.merge(parsed),
        Err(err) if required => return Err(format!("{path:?}: {err}")),
        Err(err) => warnings.push(format!("{path:?}: {err} (ignored)")),
    }
    Ok(())
}

fn compile_macros(defs: &[MacroDef], warnings: &mut Vec<String>) -> Vec<Macro> {
    let mut out = Vec::new();
    for def in defs {
        match parse_key(&def.key) {
            Some((key, mods)) => out.push(Macro {
                name: def.name.clone(),
                text: def.text.clone(),
                key,
                mods,
            }),
            None => warnings.push(format!(
                "macro {:?}: unsupported key {:?} (use f9-f12 or alt+<char>)",
                def.name, def.key
            )),
        }
    }
    out
}

/// Saves a named macro while preserving other TOML values, then returns its active binding.
pub fn save_macro(path: &Path, name: &str, spec: &str, text: &str) -> Result<Macro, String> {
    let (key, mods) = parse_key(spec)
        .ok_or_else(|| format!("unsupported key {spec:?} (use f9-f12 or alt+<char>)"))?;
    let mut document = match std::fs::read_to_string(path) {
        Ok(contents) => {
            toml::from_str::<Config>(&contents).map_err(|err| format!("{path:?}: {err}"))?;
            toml::from_str::<toml::Table>(&contents).map_err(|err| format!("{path:?}: {err}"))?
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => toml::Table::new(),
        Err(err) => return Err(format!("{path:?}: {err}")),
    };
    let definitions = document
        .entry("macros")
        .or_insert_with(|| toml::Value::Array(Vec::new()))
        .as_array_mut()
        .ok_or_else(|| format!("{path:?}: macros must be an array of tables"))?;
    let mut definition = definitions
        .iter()
        .rev()
        .find(|entry| entry.get("name").and_then(toml::Value::as_str) == Some(name))
        .and_then(toml::Value::as_table)
        .cloned()
        .unwrap_or_default();
    for (field, value) in [("name", name), ("key", spec), ("text", text)] {
        definition.insert(field.to_string(), toml::Value::String(value.to_string()));
    }
    definitions.retain(|entry| entry.get("name").and_then(toml::Value::as_str) != Some(name));
    definitions.push(toml::Value::Table(definition));
    let serialized = toml::to_string_pretty(&document)
        .map_err(|err| format!("{path:?}: cannot serialize config: {err}"))?;
    let file_name = path
        .file_name()
        .ok_or_else(|| format!("{path:?}: expected a config file path"))?;
    let temporary = path.with_file_name(format!(
        ".{}.{}.tmp",
        file_name.to_string_lossy(),
        std::process::id()
    ));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|err| format!("{path:?}: cannot create temporary config: {err}"))?;
    let result = (|| {
        std::io::Write::write_all(&mut file, serialized.as_bytes())?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temporary, path)
    })();
    if let Err(err) = result {
        let _ = std::fs::remove_file(&temporary);
        return Err(format!("{path:?}: cannot save config: {err}"));
    }
    Ok(Macro {
        name: name.to_string(),
        text: text.to_string(),
        key,
        mods,
    })
}

pub fn parse_key(spec: &str) -> Option<(KeyCode, KeyModifiers)> {
    let spec = spec.trim();
    if let Some((head, rest)) = spec.split_once('+') {
        if !head.eq_ignore_ascii_case("alt") {
            return None;
        }
        let mut chars = rest.chars();
        let ch = chars.next()?;
        if chars.next().is_some() {
            return None;
        }
        return Some((KeyCode::Char(ch), KeyModifiers::ALT));
    }
    let digits = spec
        .strip_prefix('f')
        .or_else(|| spec.strip_prefix('F'))?
        .parse::<u8>()
        .ok()?;
    if (9..=12).contains(&digits) {
        Some((KeyCode::F(digits), KeyModifiers::NONE))
    } else {
        None
    }
}

pub fn global_path() -> Option<PathBuf> {
    Some(config_dir()?.join("config.toml"))
}

/// `%APPDATA%\gtui-terms` or `$XDG_CONFIG_HOME|~/.config/gtui-terms`.
pub fn config_dir() -> Option<PathBuf> {
    let base = if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
    }?;
    Some(base.join("gtui-terms"))
}

pub fn history_path() -> Option<PathBuf> {
    Some(config_dir()?.join("history.txt"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn saving_macros_roundtrips_unrelated_values_and_updates_by_name() {
        let dir = std::env::temp_dir().join(format!("gtui-save-macro-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = write(
            &dir,
            "config.toml",
            r#"baud = 921600
custom = { enabled = true, labels = ["one", "two"] }

[[hooks]]
name = "flash"
command = "echo flash"
trigger = "ready"
extra = "keep hook metadata"

[[macros]]
name = "ping"
key = "f9"
text = "old"
extra = "keep macro metadata"

[[macros]]
name = "other"
key = "f10"
text = "untouched"
"#,
        );
        let before: toml::Table = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let saved = save_macro(&path, "ping", "alt+p", "hello \"world\"\nnext").unwrap();
        assert_eq!(saved.key, KeyCode::Char('p'));
        assert_eq!(saved.mods, KeyModifiers::ALT);
        let after: toml::Table = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        for field in ["baud", "custom", "hooks"] {
            assert_eq!(after[field], before[field]);
        }
        let entries = after["macros"].as_array().unwrap();
        assert_eq!(entries.len(), 2);
        assert!(entries.contains(&before["macros"].as_array().unwrap()[1]));
        let ping = entries
            .iter()
            .find(|entry| entry["name"].as_str() == Some("ping"))
            .unwrap();
        assert_eq!(ping["extra"].as_str(), Some("keep macro metadata"));
        assert_eq!(ping["text"].as_str(), Some("hello \"world\"\nnext"));
        save_macro(&path, "ping", "f12", "updated").unwrap();
        let mut config = Config::default();
        let mut warnings = Vec::new();
        merge_file(&mut config, &path, true, &mut warnings).unwrap();
        let macros = compile_macros(&config.macros, &mut warnings);
        assert_eq!(macros.len(), 2);
        let ping = macros.iter().find(|entry| entry.name == "ping").unwrap();
        assert_eq!(ping.key, KeyCode::F(12));
        assert_eq!(ping.text, "updated");
        assert!(warnings.is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn project_macros_replace_global_macros_with_the_same_name() {
        let mut config = Config::default();
        for contents in [
            "[[macros]]\nname = 'ping'\nkey = 'f9'\ntext = 'global'\n",
            "[[macros]]\nname = 'ping'\nkey = 'f12'\ntext = 'project'\n",
        ] {
            config.merge(toml::from_str(contents).unwrap());
        }
        let mut warnings = Vec::new();
        let macros = compile_macros(&config.macros, &mut warnings);
        assert_eq!(macros.len(), 1);
        assert_eq!(macros[0].text, "project");
        assert_eq!(macros[0].key, KeyCode::F(12));
        assert!(warnings.is_empty());
    }

    #[test]
    fn explicit_file_sets_macros_and_overrides() {
        let dir = std::env::temp_dir().join(format!("gtui-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = write(
            &dir,
            "config.toml",
            "baud = 921600\ntimestamps = true\n\n[[macros]]\nname = \"ver\"\ntext = \"AT+GMR\"\nkey = \"f9\"\n\n[[macros]]\nname = \"bad\"\ntext = \"x\"\nkey = \"f1\"\n",
        );
        let loaded = load(Some(&path)).unwrap();
        assert_eq!(loaded.config.baud, Some(921600));
        assert_eq!(loaded.macros.len(), 1);
        assert_eq!(loaded.macros[0].key, KeyCode::F(9));
        assert_eq!(loaded.warnings.len(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_explicit_file_is_an_error() {
        let err = load(Some(Path::new("definitely-missing-gtui.toml"))).unwrap_err();
        assert!(err.contains("definitely-missing-gtui.toml"));
    }

    #[test]
    fn key_specs_cover_supported_forms() {
        assert_eq!(parse_key("F10"), Some((KeyCode::F(10), KeyModifiers::NONE)));
        assert_eq!(
            parse_key("alt+1"),
            Some((KeyCode::Char('1'), KeyModifiers::ALT))
        );
        assert_eq!(parse_key("f1"), None);
        assert_eq!(parse_key("ctrl+x"), None);
        assert_eq!(parse_key("alt+ab"), None);
    }
}
