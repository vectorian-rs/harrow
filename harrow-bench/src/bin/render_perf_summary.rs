use std::path::PathBuf;

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(results_dir) = args.next() else {
        eprintln!("usage: render-perf-summary <results-dir>");
        std::process::exit(2);
    };

    if args.next().is_some() {
        eprintln!("usage: render-perf-summary <results-dir>");
        std::process::exit(2);
    }

    let results_dir = PathBuf::from(results_dir);
    match harrow_bench::perf_summary::render_results_dir(&results_dir) {
        Ok(()) => println!("Rendered {}", results_dir.display()),
        Err(err) => {
            eprintln!("failed to render {}: {err}", results_dir.display());
            std::process::exit(1);
        }
    }
}
