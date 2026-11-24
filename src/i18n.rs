//! Langue de la sortie de Virtum : français ou anglais (v1.4).
//!
//! La langue est déterminée une fois, au démarrage (`virtum::run`), avant l'analyse des
//! arguments. Chaque message est écrit avec [`tr!`](crate::tr), qui exige les deux langues :
//! un message dans une seule langue ne compile pas.

use std::cell::Cell;
use std::path::PathBuf;
use std::sync::OnceLock;

/// Langue de sortie.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    Fr,
    En,
}

impl Lang {
    /// Code enregistré dans la configuration (`fr`, `en`).
    pub fn code(self) -> &'static str {
        match self {
            Lang::Fr => "fr",
            Lang::En => "en",
        }
    }

    /// Valeur saisie par l'utilisateur, sans tenir compte de la casse.
    pub fn parse(value: &str) -> Option<Lang> {
        match value.trim().to_lowercase().as_str() {
            "fr" | "french" | "français" | "francais" => Some(Lang::Fr), // i18n-ok : valeurs saisies
            "en" | "english" => Some(Lang::En),
            _ => None,
        }
    }
}

/// Valeurs acceptées, pour les messages d'erreur.
pub const ACCEPTED: &str = "en, fr";

/// D'où vient la langue active (affiché par `virtum config get language`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LangSource {
    /// Variable `VIRTUM_LANG`.
    Env,
    /// Clé `language` du fichier de configuration.
    Config(PathBuf),
    /// Locale du système : variable lue et sa valeur.
    System { var: &'static str, value: String },
    /// Aucune source exploitable.
    Default,
}

/// Résultat de la détermination de la langue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolution {
    pub lang: Lang,
    pub source: LangSource,
    /// Avertissement à afficher (valeur de `VIRTUM_LANG` invalide).
    pub warning: Option<String>,
}

/// Variables de locale, dans l'ordre POSIX.
const LOCALE_VARS: [&str; 3] = ["LC_ALL", "LC_MESSAGES", "LANG"];

/// Détermine la langue : `VIRTUM_LANG`, puis la configuration, puis la locale du système,
/// puis l'anglais. `env` lit une variable d'environnement (paramètre, pour les tests).
pub fn resolve(
    env: impl Fn(&str) -> Option<String>,
    config: Option<(Lang, PathBuf)>,
) -> Resolution {
    let mut warning = None;
    if let Some(value) = env("VIRTUM_LANG").filter(|v| !v.trim().is_empty()) {
        match Lang::parse(&value) {
            Some(lang) => {
                return Resolution {
                    lang,
                    source: LangSource::Env,
                    warning,
                }
            }
            // La langue n'est pas encore connue : avertissement en anglais.
            None => {
                warning = Some(format!(
                    "ignoring VIRTUM_LANG='{}' (accepted: {ACCEPTED})",
                    crate::output::sanitize_terminal(&value)
                ))
            }
        }
    }
    if let Some((lang, path)) = config {
        return Resolution {
            lang,
            source: LangSource::Config(path),
            warning,
        };
    }
    for var in LOCALE_VARS {
        if let Some(value) = env(var).filter(|v| !v.is_empty()) {
            return Resolution {
                lang: lang_of_locale(&value),
                source: LangSource::System { var, value },
                warning,
            };
        }
    }
    Resolution {
        lang: Lang::En,
        source: LangSource::Default,
        warning,
    }
}

/// `fr_FR.UTF-8`, `fr_CA`, `fr@euro`, `fr` → français ; tout le reste (dont `C`) → anglais.
fn lang_of_locale(locale: &str) -> Lang {
    let language = locale
        .split(['_', '.', '@'])
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    if language == "fr" {
        Lang::Fr
    } else {
        Lang::En
    }
}

static ACTIVE: OnceLock<Resolution> = OnceLock::new();

thread_local! {
    static OVERRIDE: Cell<Option<Lang>> = const { Cell::new(None) };
}

/// Langue tant qu'aucune n'est fixée : anglais en production (erreur très précoce), français
/// dans les tests unitaires, pour qu'ils ne dépendent pas de la locale de la machine.
#[cfg(not(test))]
const UNSET: Lang = Lang::En;
#[cfg(test)]
const UNSET: Lang = Lang::Fr;

/// Fixe la langue du processus (une seule fois, au démarrage).
pub fn set_active(resolution: Resolution) {
    let _ = ACTIVE.set(resolution);
}

/// Langue active (et son origine), si elle a été fixée.
pub fn active() -> Option<&'static Resolution> {
    ACTIVE.get()
}

/// Langue active pour le fil courant.
pub fn lang() -> Lang {
    OVERRIDE
        .with(Cell::get)
        .or_else(|| ACTIVE.get().map(|r| r.lang))
        .unwrap_or(UNSET)
}

/// Exécute `f` avec une langue imposée sur le fil courant (tests, message dans une autre
/// langue que la langue active).
pub fn with_lang<R>(lang: Lang, f: impl FnOnce() -> R) -> R {
    let previous = OVERRIDE.with(|o| o.replace(Some(lang)));
    let result = f();
    OVERRIDE.with(|o| o.set(previous));
    result
}

/// Message dans la langue active : `tr!(fr: "…", en: "…", args…)`.
///
/// Les deux textes partagent les `{variables}` capturées et les arguments positionnels.
#[macro_export]
macro_rules! tr {
    (fr: $fr:literal, en: $en:literal $(,)?) => {
        match $crate::i18n::lang() {
            $crate::i18n::Lang::Fr => format!($fr),
            $crate::i18n::Lang::En => format!($en),
        }
    };
    (fr: $fr:literal, en: $en:literal, $($arg:tt)+) => {
        match $crate::i18n::lang() {
            $crate::i18n::Lang::Fr => format!($fr, $($arg)+),
            $crate::i18n::Lang::En => format!($en, $($arg)+),
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let vars: Vec<(String, String)> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |key| vars.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
    }

    #[test]
    fn parse_accepts_codes_and_names() {
        for v in ["fr", "FR", "French", "français", "Francais", " fr "] {
            assert_eq!(Lang::parse(v), Some(Lang::Fr), "{v}");
        }
        for v in ["en", "EN", "English"] {
            assert_eq!(Lang::parse(v), Some(Lang::En), "{v}");
        }
        for v in ["", "de", "klingon", "fr_FR"] {
            assert_eq!(Lang::parse(v), None, "{v}");
        }
    }

    #[test]
    fn virtum_lang_wins_over_config_and_locale() {
        let r = resolve(
            env_of(&[("VIRTUM_LANG", "en"), ("LANG", "fr_FR.UTF-8")]),
            Some((Lang::Fr, PathBuf::from("/c"))),
        );
        assert_eq!(
            (r.lang, r.source, r.warning),
            (Lang::En, LangSource::Env, None)
        );
    }

    #[test]
    fn invalid_virtum_lang_warns_and_falls_through() {
        let r = resolve(
            env_of(&[("VIRTUM_LANG", "xx"), ("LANG", "fr_FR.UTF-8")]),
            None,
        );
        assert_eq!(r.lang, Lang::Fr);
        assert!(r.warning.unwrap().contains("VIRTUM_LANG='xx'"));
        // Valeur vide : ignorée sans avertissement.
        let r = resolve(env_of(&[("VIRTUM_LANG", " "), ("LANG", "C")]), None);
        assert_eq!((r.lang, r.warning), (Lang::En, None));
    }

    #[test]
    fn config_wins_over_locale() {
        let r = resolve(
            env_of(&[("LANG", "en_US.UTF-8")]),
            Some((Lang::Fr, PathBuf::from("/home/u/.config/virtum/config.yml"))),
        );
        assert_eq!(r.lang, Lang::Fr);
        assert!(matches!(r.source, LangSource::Config(_)));
    }

    #[test]
    fn locale_follows_posix_order() {
        let r = resolve(
            env_of(&[("LC_ALL", "en_US.UTF-8"), ("LANG", "fr_FR.UTF-8")]),
            None,
        );
        assert_eq!(r.lang, Lang::En);
        assert_eq!(
            r.source,
            LangSource::System {
                var: "LC_ALL",
                value: "en_US.UTF-8".into()
            }
        );
        let r = resolve(
            env_of(&[("LC_ALL", ""), ("LC_MESSAGES", "fr_CA"), ("LANG", "C")]),
            None,
        );
        assert_eq!(r.lang, Lang::Fr);
    }

    #[test]
    fn locale_values() {
        for (locale, lang) in [
            ("fr_FR.UTF-8", Lang::Fr),
            ("fr_BE", Lang::Fr),
            ("fr@euro", Lang::Fr),
            ("fr", Lang::Fr),
            ("FR_fr", Lang::Fr),
            ("en_US.UTF-8", Lang::En),
            ("de_DE.UTF-8", Lang::En),
            ("C", Lang::En),
            ("C.UTF-8", Lang::En),
            ("POSIX", Lang::En),
            ("fy_NL", Lang::En),
        ] {
            assert_eq!(lang_of_locale(locale), lang, "{locale}");
        }
    }

    #[test]
    fn no_source_defaults_to_english() {
        let r = resolve(env_of(&[]), None);
        assert_eq!((r.lang, r.source), (Lang::En, LangSource::Default));
    }

    #[test]
    fn tr_formats_in_the_active_language() {
        let id = "vm-1";
        let n = 3;
        let msg =
            || tr!(fr: "VM {id} introuvable ({} essais)", en: "VM {id} not found ({} tries)", n);
        assert_eq!(with_lang(Lang::Fr, msg), "VM vm-1 introuvable (3 essais)");
        assert_eq!(with_lang(Lang::En, msg), "VM vm-1 not found (3 tries)");
        assert_eq!(with_lang(Lang::En, || tr!(fr: "oui", en: "yes")), "yes");
        // Le remplacement est retiré après l'appel.
        with_lang(Lang::En, || ());
        assert_eq!(lang(), UNSET);
    }
}
