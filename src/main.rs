fn main() {
    if let Err(err) = virtum::run() {
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}
