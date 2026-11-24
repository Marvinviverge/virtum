fn main() {
    if let Err(err) = virtum::run() {
        eprintln!(
            "{}",
            virtum::tr!(fr: "erreur : {err:#}", en: "error: {err:#}")
        );
        std::process::exit(1);
    }
}
