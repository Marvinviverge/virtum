//! Sortie préfixée par machine pendant un `virtum up` parallèle (v0.9).
//!
//! Chaque machine est créée dans son propre fil d'exécution : le préfixe est propre au
//! fil (`thread_local`). Sans préfixe (cas général), `verr!` / `vout!` se comportent
//! exactement comme `eprintln!` / `println!`.

use std::cell::RefCell;

thread_local! {
    static PREFIX: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Fixe (ou retire) le préfixe des lignes émises par ce fil.
pub fn set_prefix(prefix: Option<String>) {
    PREFIX.with(|p| *p.borrow_mut() = prefix);
}

/// Texte avec le préfixe du fil sur chaque ligne.
pub fn prefixed(text: &str) -> String {
    PREFIX.with(|p| match p.borrow().as_deref() {
        None => text.to_string(),
        Some(prefix) if text.is_empty() => prefix.trim_end().to_string(),
        Some(prefix) => text
            .lines()
            .map(|line| format!("{prefix}{line}"))
            .collect::<Vec<_>>()
            .join("\n"),
    })
}

/// Rend inoffensif un texte venu d'un autre programme (stderr d'une commande dans la VM,
/// sortie d'un outil) avant de l'afficher : les caractères de contrôle autres que le saut
/// de ligne et la tabulation, dont ESC qui ouvre les séquences de terminal (changer le
/// titre, effacer l'écran, réécrire la ligne, OSC 52 vers le presse-papiers), et les
/// caractères de direction bidirectionnelle sont affichés sous forme `\u{1b}`.
pub fn sanitize_terminal(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        let bidi = matches!(c, '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}');
        if (c.is_control() && c != '\n' && c != '\t') || bidi {
            out.push_str(&c.escape_unicode().to_string());
        } else {
            out.push(c);
        }
    }
    out
}

/// `eprintln!` préfixé par la machine en cours (fil courant).
///
/// Forme bilingue : `verr!(fr: "…", en: "…", args…)` (voir [`tr!`](crate::tr)).
#[macro_export]
macro_rules! verr {
    (fr: $fr:literal, en: $en:literal $(, $($arg:tt)+)?) => {
        eprintln!("{}", $crate::output::prefixed(&$crate::tr!(fr: $fr, en: $en $(, $($arg)+)?)))
    };
    () => {
        eprintln!("{}", $crate::output::prefixed(""))
    };
    ($($arg:tt)*) => {
        eprintln!("{}", $crate::output::prefixed(&format!($($arg)*)))
    };
}

/// `println!` préfixé par la machine en cours (fil courant).
///
/// Forme bilingue : `vout!(fr: "…", en: "…", args…)`.
#[macro_export]
macro_rules! vout {
    (fr: $fr:literal, en: $en:literal $(, $($arg:tt)+)?) => {
        println!("{}", $crate::output::prefixed(&$crate::tr!(fr: $fr, en: $en $(, $($arg)+)?)))
    };
    () => {
        println!("{}", $crate::output::prefixed(""))
    };
    ($($arg:tt)*) => {
        println!("{}", $crate::output::prefixed(&format!($($arg)*)))
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_terminal_neutralises_escape_sequences() {
        assert_eq!(sanitize_terminal("ok\n\tligne"), "ok\n\tligne");
        assert_eq!(
            sanitize_terminal("\u{1b}]0;titre\u{7}\u{1b}[2J\r\u{9b}x"),
            "\\u{1b}]0;titre\\u{7}\\u{1b}[2J\\u{d}\\u{9b}x"
        );
        assert_eq!(sanitize_terminal("a\u{202e}b"), "a\\u{202e}b");
        assert!(!sanitize_terminal("\u{1b}[31mrouge").contains('\u{1b}'));
        assert_eq!(sanitize_terminal("été ✓"), "été ✓");
    }

    #[test]
    fn prefix_is_per_thread_and_per_line() {
        assert_eq!(prefixed("a\nb"), "a\nb");
        set_prefix(Some("[web] ".into()));
        assert_eq!(prefixed("a\nb"), "[web] a\n[web] b");
        assert_eq!(prefixed(""), "[web]");
        std::thread::spawn(|| assert_eq!(prefixed("x"), "x"))
            .join()
            .unwrap();
        set_prefix(None);
        assert_eq!(prefixed("x"), "x");
    }
}
