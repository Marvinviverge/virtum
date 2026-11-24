//! Configuration de l'utilisateur : `$XDG_CONFIG_HOME/virtum/config.yml` (v1.4).
//!
//! Une seule clé pour l'instant : `language: fr|en`. Fichier par utilisateur, jamais dans
//! `/var/lib/virtum` (partagé). Lecture d'un fichier régulier de l'utilisateur, sans suivre
//! de lien ; écriture atomique (garantie G4 de SECURITY.md).

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::cli::{ConfigAction, ConfigKey};
use crate::i18n::{Lang, LangSource};
use crate::tr;

const FILE_NAME: &str = "config.yml";

/// Chemin du fichier de configuration, si `$HOME` ou `$XDG_CONFIG_HOME` est connu.
pub fn config_path() -> Option<PathBuf> {
    path_from(
        std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from),
        dirs::home_dir(),
    )
}

/// `$XDG_CONFIG_HOME/virtum/config.yml` s'il est absolu (spécification XDG), sinon
/// `$HOME/.config/virtum/config.yml`.
fn path_from(xdg: Option<PathBuf>, home: Option<PathBuf>) -> Option<PathBuf> {
    let base = xdg
        .filter(|p| p.is_absolute())
        .or_else(|| home.map(|h| h.join(".config")))?;
    Some(base.join("virtum").join(FILE_NAME))
}

/// Problème rencontré à la lecture : la configuration est ignorée, jamais corrigée.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigWarning {
    /// Fichier illisible, lien symbolique ou fichier d'un autre utilisateur.
    Unreadable { path: PathBuf, reason: String },
    /// YAML invalide ou contenu qui n'est pas une table de clés.
    Invalid { path: PathBuf },
    /// Valeur de `language` inconnue.
    BadLanguage { path: PathBuf, value: String },
    /// Clé inconnue (ignorée, et non conservée au prochain `config set`).
    UnknownKey { path: PathBuf, key: String },
}

impl ConfigWarning {
    /// Message dans la langue active.
    pub fn message(&self) -> String {
        match self {
            ConfigWarning::Unreadable { path, reason } => {
                let path = display_path(path);
                tr!(
                    fr: "configuration {path} ignorée : {reason}",
                    en: "ignoring configuration {path}: {reason}",
                )
            }
            ConfigWarning::Invalid { path } => {
                let path = display_path(path);
                tr!(
                    fr: "configuration {path} ignorée : YAML invalide (attendu : `language: fr` ou `language: en`)",
                    en: "ignoring configuration {path}: invalid YAML (expected `language: fr` or `language: en`)",
                )
            }
            ConfigWarning::BadLanguage { path, value } => {
                let path = display_path(path);
                let accepted = crate::i18n::ACCEPTED;
                tr!(
                    fr: "langue « {value} » inconnue dans {path} (valeurs acceptées : {accepted}) : ignorée",
                    en: "unknown language '{value}' in {path} (accepted: {accepted}): ignored",
                )
            }
            ConfigWarning::UnknownKey { path, key } => {
                let path = display_path(path);
                tr!(
                    fr: "clé « {key} » inconnue dans {path} : ignorée",
                    en: "unknown key '{key}' in {path}: ignored",
                )
            }
        }
    }
}

/// Configuration lue (vide si le fichier est absent ou ignoré).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Loaded {
    pub path: Option<PathBuf>,
    pub language: Option<Lang>,
    pub warnings: Vec<ConfigWarning>,
}

/// Lit la configuration de l'utilisateur courant. Ne fait jamais échouer la commande.
pub fn load() -> Loaded {
    match config_path() {
        Some(path) => load_from(&path),
        None => Loaded::default(),
    }
}

fn load_from(path: &Path) -> Loaded {
    let mut loaded = Loaded {
        path: Some(path.to_path_buf()),
        ..Loaded::default()
    };
    let content = match crate::fsutil::read_owned(path) {
        Ok(Some(content)) => content,
        Ok(None) => return loaded,
        Err(err) => {
            loaded.warnings.push(ConfigWarning::Unreadable {
                path: path.to_path_buf(),
                reason: crate::output::sanitize_terminal(&format!("{err:#}")),
            });
            return loaded;
        }
    };
    let value: serde_json::Value = if content.trim().is_empty() {
        serde_json::Value::Object(Default::default())
    } else {
        match serde_saphyr::from_str(&content) {
            Ok(v) => v,
            Err(_) => {
                loaded.warnings.push(ConfigWarning::Invalid {
                    path: path.to_path_buf(),
                });
                return loaded;
            }
        }
    };
    let Some(map) = value.as_object() else {
        loaded.warnings.push(ConfigWarning::Invalid {
            path: path.to_path_buf(),
        });
        return loaded;
    };
    for (key, value) in map {
        if key != "language" {
            loaded.warnings.push(ConfigWarning::UnknownKey {
                path: path.to_path_buf(),
                key: crate::output::sanitize_terminal(key),
            });
            continue;
        }
        match value.as_str().and_then(Lang::parse) {
            Some(lang) => loaded.language = Some(lang),
            None => loaded.warnings.push(ConfigWarning::BadLanguage {
                path: path.to_path_buf(),
                value: crate::output::sanitize_terminal(&value_text(value)),
            }),
        }
    }
    loaded
}

fn value_text(value: &serde_json::Value) -> String {
    value
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| value.to_string())
}

/// Enregistre la langue (`config set language`). Refuse d'écrire à travers un lien
/// symbolique ou dans le fichier d'un autre utilisateur.
pub fn set_language(lang: Lang) -> Result<PathBuf> {
    let path = require_path()?;
    set_language_at(&path, lang)?;
    Ok(path)
}

fn set_language_at(path: &Path, lang: Lang) -> Result<()> {
    refuse_foreign_file(path)?;
    if let Some(dir) = path.parent() {
        create_config_dir(dir)?;
    }
    let content = format!("language: {}\n", lang.code());
    crate::fsutil::write_atomic(path, content.as_bytes(), 0o644)
}

/// Retire la langue enregistrée (`config unset language`). Le fichier, qui n'a pas d'autre
/// clé, est supprimé. Renvoie `false` s'il n'y avait rien à retirer.
pub fn unset_language() -> Result<bool> {
    unset_language_at(&require_path()?)
}

fn unset_language_at(path: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).with_context(|| {
            let path = path.display();
            tr!(fr: "Impossible de lire {path}", en: "Cannot read {path}")
        }),
        Ok(_) => {
            refuse_foreign_file(path)?;
            std::fs::remove_file(path).with_context(|| {
                let path = path.display();
                tr!(fr: "Impossible de supprimer {path}", en: "Cannot remove {path}")
            })?;
            Ok(true)
        }
    }
}

fn require_path() -> Result<PathBuf> {
    match config_path() {
        Some(path) => Ok(path),
        None => bail!(tr!(
            fr: "Dossier personnel introuvable : définissez HOME ou XDG_CONFIG_HOME.",
            en: "Home directory not found: set HOME or XDG_CONFIG_HOME.",
        )),
    }
}

/// Un fichier existant doit être un fichier régulier de l'utilisateur courant.
fn refuse_foreign_file(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) => crate::fsutil::ensure_owned_regular_file(path, &meta),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| {
            let path = path.display();
            tr!(fr: "Impossible de lire {path}", en: "Cannot read {path}")
        }),
    }
}

fn create_config_dir(dir: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o755)
        .create(dir)
        .with_context(|| {
            let dir = dir.display();
            tr!(fr: "Impossible de créer {dir}", en: "Cannot create {dir}")
        })
}

/// Nom d'une langue, écrit dans la langue active.
fn lang_name(lang: Lang) -> String {
    match lang {
        Lang::Fr => tr!(fr: "français", en: "French"),
        Lang::En => tr!(fr: "anglais", en: "English"),
    }
}

/// Chemin affiché avec `~` pour le dossier personnel.
fn display_path(path: &Path) -> String {
    if let Some(home) = dirs::home_dir() {
        if let Ok(rest) = path.strip_prefix(&home) {
            return format!("~/{}", rest.display());
        }
    }
    path.display().to_string()
}

/// Origine de la langue, dans la langue active (`virtum config get language`).
fn source_text(source: &LangSource) -> String {
    match source {
        LangSource::Env => tr!(fr: "variable VIRTUM_LANG", en: "VIRTUM_LANG variable"),
        LangSource::Config(path) => {
            let path = display_path(path);
            tr!(fr: "configuration : {path}", en: "configuration: {path}")
        }
        LangSource::System { var, value } => {
            let value = crate::output::sanitize_terminal(value);
            tr!(fr: "langue du système : {var}={value}", en: "system language: {var}={value}")
        }
        LangSource::Default => tr!(fr: "défaut", en: "default"),
    }
}

fn env_var(key: &str) -> Option<String> {
    std::env::var(key).ok()
}

/// `virtum config set|get|unset language`.
pub fn command(action: ConfigAction) -> Result<()> {
    match action {
        ConfigAction::Set {
            key: ConfigKey::Language,
            value,
        } => {
            let Some(lang) = Lang::parse(&value) else {
                let value = crate::output::sanitize_terminal(&value);
                let accepted = crate::i18n::ACCEPTED;
                bail!(tr!(
                    fr: "Langue « {value} » inconnue. Valeurs acceptées : {accepted}.",
                    en: "Unknown language '{value}'. Accepted values: {accepted}.",
                ));
            };
            set_language(lang)?;
            // Confirmation dans la nouvelle langue.
            crate::i18n::with_lang(lang, || {
                println!(
                    "{}",
                    tr!(fr: "Langue réglée sur le français.", en: "Language set to English.")
                );
                let env_lang = env_var("VIRTUM_LANG").and_then(|v| Lang::parse(&v));
                if let Some(env_lang) = env_lang.filter(|l| *l != lang) {
                    let code = env_lang.code();
                    println!(
                        "{}",
                        tr!(
                            fr: "VIRTUM_LANG={code} reste prioritaire dans ce terminal.",
                            en: "VIRTUM_LANG={code} remains in effect in this shell.",
                        )
                    );
                }
            });
        }
        ConfigAction::Get {
            key: ConfigKey::Language,
        } => {
            let resolution = match crate::i18n::active() {
                Some(r) => r.clone(),
                None => {
                    let loaded = load();
                    let config = loaded.language.zip(loaded.path);
                    crate::i18n::resolve(env_var, config)
                }
            };
            println!(
                "{} ({})",
                resolution.lang.code(),
                source_text(&resolution.source)
            );
        }
        ConfigAction::Unset {
            key: ConfigKey::Language,
        } => {
            if !unset_language()? {
                println!(
                    "{}",
                    tr!(fr: "Aucun réglage de langue à retirer.", en: "No language setting to remove.")
                );
                return Ok(());
            }
            // Langue du système, désormais utilisée (hors VIRTUM_LANG ponctuelle).
            let system =
                crate::i18n::resolve(|k| if k == "VIRTUM_LANG" { None } else { env_var(k) }, None)
                    .lang;
            crate::i18n::with_lang(system, || {
                let name = lang_name(system);
                println!(
                    "{}",
                    tr!(
                        fr: "Réglage de langue retiré : Virtum suit la langue du système ({name}).",
                        en: "Language setting removed: using the system language ({name}).",
                    )
                );
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_and_language_names_are_translated() {
        use crate::i18n::with_lang;
        let sys = LangSource::System {
            var: "LANG",
            value: "fr_FR.UTF-8".into(),
        };
        assert_eq!(
            with_lang(Lang::En, || source_text(&sys)),
            "system language: LANG=fr_FR.UTF-8"
        );
        assert_eq!(
            with_lang(Lang::Fr, || source_text(&sys)),
            "langue du système : LANG=fr_FR.UTF-8"
        );
        assert_eq!(with_lang(Lang::En, || lang_name(Lang::Fr)), "French");
        assert_eq!(with_lang(Lang::Fr, || lang_name(Lang::En)), "anglais");
        assert_eq!(display_path(Path::new("/etc/x")), "/etc/x");
    }

    fn dir(name: &str) -> PathBuf {
        crate::test_env::private_temp_dir(name)
    }

    #[test]
    fn path_follows_xdg_then_home() {
        let home = Some(PathBuf::from("/home/u"));
        assert_eq!(
            path_from(None, home.clone()).unwrap(),
            PathBuf::from("/home/u/.config/virtum/config.yml")
        );
        assert_eq!(
            path_from(Some("/x/cfg".into()), home.clone()).unwrap(),
            PathBuf::from("/x/cfg/virtum/config.yml")
        );
        // XDG relatif : ignoré (spécification XDG).
        assert_eq!(
            path_from(Some("rel".into()), home).unwrap(),
            PathBuf::from("/home/u/.config/virtum/config.yml")
        );
        assert_eq!(path_from(None, None), None);
    }

    #[test]
    fn set_then_load_then_unset() {
        let d = dir("ucfg-roundtrip");
        let path = d.join("virtum").join("config.yml");
        assert_eq!(load_from(&path).language, None);
        set_language_at(&path, Lang::En).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "language: en\n");
        let loaded = load_from(&path);
        assert_eq!((loaded.language, loaded.warnings), (Some(Lang::En), vec![]));
        set_language_at(&path, Lang::Fr).unwrap();
        assert_eq!(load_from(&path).language, Some(Lang::Fr));
        assert!(unset_language_at(&path).unwrap());
        assert!(!path.exists());
        assert!(!unset_language_at(&path).unwrap());
    }

    #[test]
    fn symlink_is_refused_and_ignored() {
        use std::os::unix::fs::symlink;
        let d = dir("ucfg-symlink");
        let target = d.join("cible");
        std::fs::create_dir_all(d.join("virtum")).unwrap();
        let path = d.join("virtum").join("config.yml");
        symlink(&target, &path).unwrap();
        assert!(set_language_at(&path, Lang::En).is_err());
        assert!(!target.exists(), "la cible du lien ne doit pas être créée");
        let loaded = load_from(&path);
        assert_eq!(loaded.language, None);
        assert!(matches!(
            loaded.warnings[..],
            [ConfigWarning::Unreadable { .. }]
        ));
    }

    #[test]
    fn invalid_content_is_ignored_with_a_warning() {
        let d = dir("ucfg-invalid");
        let path = d.join("config.yml");
        for (content, expected) in [
            ("language: [", "Invalid"),
            ("- en\n", "Invalid"),
            ("language: klingon\n", "BadLanguage"),
            ("language: 3\n", "BadLanguage"),
            ("language: fr\ntheme: dark\n", "UnknownKey"),
        ] {
            std::fs::write(&path, content).unwrap();
            let loaded = load_from(&path);
            let kind = format!("{:?}", loaded.warnings);
            assert!(kind.contains(expected), "{content:?} → {kind}");
        }
        // Clé inconnue : la langue valide est quand même prise en compte.
        assert_eq!(load_from(&path).language, Some(Lang::Fr));
        std::fs::write(&path, "").unwrap();
        assert_eq!(
            load_from(&path),
            Loaded {
                path: Some(path.clone()),
                ..Loaded::default()
            }
        );
    }

    #[test]
    fn warnings_are_translated() {
        let w = ConfigWarning::UnknownKey {
            path: PathBuf::from("/c.yml"),
            key: "theme".into(),
        };
        assert_eq!(
            crate::i18n::with_lang(Lang::En, || w.message()),
            "unknown key 'theme' in /c.yml: ignored"
        );
        assert_eq!(
            crate::i18n::with_lang(Lang::Fr, || w.message()),
            "clé « theme » inconnue dans /c.yml : ignorée"
        );
    }
}
