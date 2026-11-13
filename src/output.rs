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

/// `eprintln!` préfixé par la machine en cours (fil courant).
#[macro_export]
macro_rules! verr {
    () => {
        eprintln!("{}", $crate::output::prefixed(""))
    };
    ($($arg:tt)*) => {
        eprintln!("{}", $crate::output::prefixed(&format!($($arg)*)))
    };
}

/// `println!` préfixé par la machine en cours (fil courant).
#[macro_export]
macro_rules! vout {
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
