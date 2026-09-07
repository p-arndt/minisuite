// Thin wrapper around the minimail library: parse, prepare, serve. All the
// logic (and every error path) lives in src/lib.rs so the minisuite launcher
// can do exactly the same thing without a process boundary.

fn main() {
    let cfg = match minimail::parse_args(std::env::args().skip(1)) {
        Ok(c) => c,
        Err(e) => {
            if e.code == 0 {
                println!("{}", e.message)
            } else {
                eprintln!("{}", e.message)
            }
            std::process::exit(e.code)
        }
    };
    let ready = match minimail::prepare(cfg) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("minimail: {e}");
            std::process::exit(1)
        }
    };
    eprint!("{}", ready.banner());
    if let Err(e) = ready.serve() {
        eprintln!("minimail: {e}");
        std::process::exit(1);
    }
}
