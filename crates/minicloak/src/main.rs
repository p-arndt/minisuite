// minicloak: a tiny, dependency-free OIDC provider for local development.
//
// All of the work lives in the library (src/lib.rs) so that the minisuite
// launcher can run the same server in a thread. This file only maps the
// library's errors onto exit codes.

fn main() {
    let cfg = match minicloak::parse_args(std::env::args().skip(1)) {
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

    let ready = match minicloak::prepare(cfg) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("minicloak: {e}");
            std::process::exit(1);
        }
    };

    eprint!("{}", ready.banner());

    if let Err(e) = ready.serve() {
        eprintln!("minicloak: {e}");
        std::process::exit(1);
    }
}
