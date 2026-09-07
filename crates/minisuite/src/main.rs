// Thin wrapper around the minisuite library: parse, run. This is the only file
// in the crate that knows about exit codes; everything else returns a Result so
// the launcher stays testable and never kills the process from a helper.

fn main() {
    let cfg = match minisuite::parse_args(std::env::args().skip(1)) {
        Ok(c) => c,
        Err(e) => {
            // code 0 is --help/--version: the message is what the user asked for.
            if e.code == 0 {
                println!("{}", e.message);
            } else {
                eprintln!("{}", e.message);
            }
            std::process::exit(e.code);
        }
    };

    if let Err(e) = minisuite::run(cfg) {
        // `e` already carries the `minisuite: <service>: ...` prefix, because
        // only the launcher knows which of the three servers failed.
        eprintln!("{}", e);
        std::process::exit(1);
    }
}
