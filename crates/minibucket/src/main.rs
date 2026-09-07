// Thin CLI wrapper. All of the work lives in the library so that the
// `minisuite` launcher can drive minibucket the same way from a thread.

fn main() {
    let cfg = match minibucket::parse_args(std::env::args().skip(1)) {
        Ok(c) => c,
        Err(e) => {
            if e.code == 0 {
                println!("{}", e.message)
            } else {
                eprintln!("{}", e.message)
            };
            std::process::exit(e.code)
        }
    };
    let ready = match minibucket::prepare(cfg) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("minibucket: {e}");
            std::process::exit(1)
        }
    };
    eprint!("{}", ready.banner());
    if let Err(e) = ready.serve() {
        eprintln!("minibucket: {e}");
        std::process::exit(1);
    }
}
