//! Garde-fou de la v1.4 : tout message affiché par Virtum existe en français et en anglais.
//!
//! Un message bilingue s'écrit `tr!(fr: "…", en: "…")` (ou `verr!`/`vout!` sous la même
//! forme). Ce test analyse `src/*.rs` et signale :
//! - une chaîne contenant du texte passée directement à `bail!`, `anyhow!`, `ensure!`,
//!   `.context(`, `.with_context(`, `verr!`, `vout!`, `println!`, `eprintln!`, `print!`,
//!   `eprint!` ;
//! - toute chaîne contenant une lettre accentuée hors d'un message bilingue (texte français
//!   oublié).
//!
//! Les formats purs (`"{}"`, `"{err:#}"`) sont acceptés. Une ligne marquée `// i18n-ok`
//! est ignorée (texte volontairement non traduit, ex. en-têtes de colonnes lus par des
//! scripts). Le code de test (`#[cfg(test)]` en début de ligne, jusqu'à la fin du fichier)
//! n'est pas analysé.

use std::path::Path;

/// Fichiers pas encore migrés (v1.4, étapes 61 à 63). La liste se vide à chaque étape ;
/// à la fin de l'étape 63, elle est vide et le test est strict.
const TOLERATED: &[&str] = &[
    "ansible.rs",
    "apply.rs",
    "bridge_network.rs",
    "cloud_init.rs",
    "command.rs",
    "config.rs",
    "disk.rs",
    "doctor.rs",
    "firewall.rs",
    "fsutil.rs",
    "images.rs",
    "inventory.rs",
    "lab.rs",
    "lab_diff.rs",
    "lab_network.rs",
    "lib.rs",
    "libvirt.rs",
    "libvirt_access.rs",
    "metadata.rs",
    "mounts.rs",
    "ownership.rs",
    "parallel.rs",
    "plan.rs",
    "port_forward.rs",
    "procs.rs",
    "ps.rs",
    "share_access.rs",
    "snapshot.rs",
    "ssh.rs",
    "test_env.rs",
    "virtumfile.rs",
    "vm.rs",
];

const SINKS: &[&str] = &[
    "bail!(",
    "anyhow!(",
    "ensure!(",
    ".context(",
    ".with_context(",
    "verr!(",
    "vout!(",
    "println!(",
    "eprintln!(",
    "print!(",
    "eprint!(",
];

/// Chaîne littérale du source.
struct Literal {
    start: usize,
    end: usize,
    line: usize,
    text: String,
}

/// Source découpé : `code` reprend le fichier caractère par caractère, commentaires et
/// chaînes remplacés par des espaces (sauts de ligne conservés).
struct Scanned {
    code: Vec<char>,
    literals: Vec<Literal>,
}

fn scan(src: &str) -> Scanned {
    let chars: Vec<char> = src.chars().collect();
    let mut code = chars.clone();
    let mut literals = Vec::new();
    let mut line = 1;
    let mut i = 0;
    let blank = |code: &mut Vec<char>, from: usize, to: usize| {
        for c in code.iter_mut().take(to).skip(from) {
            if *c != '\n' {
                *c = ' ';
            }
        }
    };
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        if c == '\n' {
            line += 1;
            i += 1;
        } else if c == '/' && next == Some('/') {
            let start = i;
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            blank(&mut code, start, i);
        } else if c == '/' && next == Some('*') {
            let start = i;
            i += 2;
            while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                if chars[i] == '\n' {
                    line += 1;
                }
                i += 1;
            }
            i = (i + 2).min(chars.len());
            blank(&mut code, start, i);
        } else if c == '"'
            || (c == 'r' && matches!(next, Some('"') | Some('#')) && is_raw_start(&chars, i))
        {
            let start = i;
            let start_line = line;
            let (text, end) = if c == 'r' {
                read_raw(&chars, i)
            } else {
                read_string(&chars, i)
            };
            line += chars[start..end].iter().filter(|&&ch| ch == '\n').count();
            blank(&mut code, start, end);
            literals.push(Literal {
                start,
                end,
                line: start_line,
                text,
            });
            i = end;
        } else if c == '\'' {
            // Caractère littéral ('x', '\n') ou durée de vie ('a) : on saute le caractère.
            if next == Some('\\') {
                let start = i;
                i += 2;
                while i < chars.len() && chars[i] != '\'' {
                    i += 1;
                }
                i += 1;
                blank(&mut code, start, i);
            } else if chars.get(i + 2) == Some(&'\'') {
                blank(&mut code, i, i + 3);
                i += 3;
            } else {
                i += 1;
            }
        } else {
            i += 1;
        }
    }
    Scanned { code, literals }
}

/// `r"…"` ou `r#"…"#` (et non un identifiant qui finit par `r`).
fn is_raw_start(chars: &[char], i: usize) -> bool {
    let prev_ok = i == 0 || !(chars[i - 1].is_alphanumeric() || chars[i - 1] == '_');
    let mut j = i + 1;
    while chars.get(j) == Some(&'#') {
        j += 1;
    }
    prev_ok && chars.get(j) == Some(&'"')
}

fn read_string(chars: &[char], start: usize) -> (String, usize) {
    let mut i = start + 1;
    let mut text = String::new();
    while i < chars.len() && chars[i] != '"' {
        if chars[i] == '\\' {
            i += 1;
        }
        if let Some(&c) = chars.get(i) {
            text.push(c);
        }
        i += 1;
    }
    (text, i + 1)
}

fn read_raw(chars: &[char], start: usize) -> (String, usize) {
    let mut i = start + 1;
    let mut hashes = 0;
    while chars[i] == '#' {
        hashes += 1;
        i += 1;
    }
    i += 1;
    let mut text = String::new();
    while i < chars.len() {
        if chars[i] == '"' && (0..hashes).all(|k| chars.get(i + 1 + k) == Some(&'#')) {
            return (text, i + 1 + hashes);
        }
        text.push(chars[i]);
        i += 1;
    }
    (text, i)
}

/// Fin (exclue) de l'appel dont la parenthèse ouvrante est en `open`.
fn closing_paren(code: &[char], open: usize) -> usize {
    let mut depth = 0;
    for (i, &c) in code.iter().enumerate().skip(open) {
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => {
                depth -= 1;
                if depth == 0 {
                    return i + 1;
                }
            }
            _ => {}
        }
    }
    code.len()
}

fn find_all(code: &[char], pattern: &str) -> Vec<usize> {
    let pat: Vec<char> = pattern.chars().collect();
    (0..code.len().saturating_sub(pat.len() - 1))
        .filter(|&i| code[i..i + pat.len()] == pat[..])
        .collect()
}

/// Texte réel d'une chaîne de format, sans les `{…}`.
fn has_words(text: &str) -> bool {
    let mut depth = 0;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' if chars.peek() == Some(&'{') => {
                chars.next();
            }
            '{' => depth += 1,
            '}' if depth > 0 => depth -= 1,
            c if depth == 0 && c.is_alphabetic() => return true,
            _ => {}
        }
    }
    false
}

/// Violations d'un fichier : (ligne, extrait).
fn violations(src: &str) -> Vec<(usize, String)> {
    let cut = src
        .lines()
        .scan(0usize, |offset, l| {
            let start = *offset;
            *offset += l.chars().count() + 1;
            Some((start, l))
        })
        .find(|(_, l)| l.starts_with("#[cfg(test)]"))
        .map(|(start, _)| start);
    let src: String = match cut {
        Some(n) => src.chars().take(n).collect(),
        None => src.to_string(),
    };
    let lines: Vec<&str> = src.lines().collect();
    let scanned = scan(&src);
    let code = &scanned.code;

    // Appels bilingues : `xxx!(fr: …)`.
    let mut bilingual: Vec<(usize, usize)> = Vec::new();
    for open in find_all(code, "!(").into_iter().map(|i| i + 1) {
        let after: String = code[open + 1..]
            .iter()
            .skip_while(|c| c.is_whitespace())
            .take(3)
            .collect();
        if after == "fr:" {
            bilingual.push((open, closing_paren(code, open)));
        }
    }
    let in_bilingual = |lit: &Literal| {
        bilingual
            .iter()
            .any(|&(a, b)| lit.start > a && lit.end <= b)
    };
    let ignored = |lit: &Literal| {
        lines
            .get(lit.line - 1)
            .is_some_and(|l| l.contains("i18n-ok"))
    };

    let mut out = Vec::new();
    for sink in SINKS {
        for at in find_all(code, sink) {
            // `eprint!(` est aussi la fin de `veprint!(`… : on exige un début de mot.
            if at > 0 && sink.starts_with(char::is_alphabetic) {
                let prev = code[at - 1];
                if prev.is_alphanumeric() || prev == '_' {
                    continue;
                }
            }
            let open = at + sink.len() - 1;
            let close = closing_paren(code, open);
            for lit in &scanned.literals {
                if lit.start > open
                    && lit.end <= close
                    && !in_bilingual(lit)
                    && !ignored(lit)
                    && has_words(&lit.text)
                {
                    out.push((lit.line, format!("{sink} \"{}\"", lit.text)));
                }
            }
        }
    }
    for lit in &scanned.literals {
        if lit.text.chars().any(|c| c.is_alphabetic() && !c.is_ascii())
            && !in_bilingual(lit)
            && !ignored(lit)
        {
            out.push((lit.line, format!("accent \"{}\"", lit.text)));
        }
    }
    out.sort();
    out.dedup();
    out
}

#[test]
fn every_message_is_bilingual() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut report = Vec::new();
    let mut seen = Vec::new();
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        seen.push(name.clone());
        if TOLERATED.contains(&name.as_str()) {
            continue;
        }
        let src = std::fs::read_to_string(&path).unwrap();
        for (line, what) in violations(&src) {
            report.push(format!("src/{name}:{line}: {what}"));
        }
    }
    for name in TOLERATED {
        assert!(
            seen.iter().any(|s| s == name),
            "TOLERATED cite src/{name}, qui n'existe plus : retirez-le de la liste"
        );
    }
    assert!(
        report.is_empty(),
        "Messages à écrire avec tr!(fr: …, en: …) :\n{}",
        report.join("\n")
    );
}

#[test]
fn scanner_detects_untranslated_messages() {
    let src = r#####"
fn f() -> Result<()> {
    // bail!("commentaire ignoré");
    verr!(fr: "Démarrage de {vm}…", en: "Starting {vm}…");
    let m = tr!(fr: "VM « {id} » introuvable", en: "VM '{id}' not found");
    bail!("{}", tr!(fr: "Échec", en: "Failed"));
    eprintln!("{err:#}");
    println!("{:<10} {}", "NAME", "LAB"); // i18n-ok
    let a = 'x';
    let r = r#"texte "brut" créé"#;
    bail!(
        "Instance {reference} introuvable"
    );
    run().with_context(|| format!("Cannot read {}", path))?;
    let s = "image vérifiée";
    Ok(())
}
#[cfg(test)]
mod tests { fn t() { bail!("test ignoré"); } }
"#####;
    let found: Vec<String> = violations(src)
        .into_iter()
        .map(|(l, w)| format!("{l}: {w}"))
        .collect();
    assert_eq!(
        found,
        vec![
            "10: accent \"texte \"brut\" créé\"",
            "12: bail!( \"Instance {reference} introuvable\"",
            "14: .with_context( \"Cannot read {}\"",
            "15: accent \"image vérifiée\"",
        ],
        "{found:#?}"
    );
}
