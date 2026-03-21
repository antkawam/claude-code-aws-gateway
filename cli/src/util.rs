use indicatif::{ProgressBar, ProgressStyle};

/// Create a spinner-style progress bar with a message
#[allow(dead_code)]
pub fn spinner(msg: &str) -> ProgressBar {
    let pb = ProgressBar::new_spinner();
    pb.set_style(
        ProgressStyle::default_spinner()
            .template("{spinner:.cyan} {msg}")
            .expect("valid template"),
    );
    pb.set_message(msg.to_string());
    pb.enable_steady_tick(std::time::Duration::from_millis(100));
    pb
}

/// Print a success message
pub fn success(msg: &str) {
    eprintln!("\x1b[32m[ok]\x1b[0m {msg}");
}

/// Print an info message
pub fn info(msg: &str) {
    eprintln!("\x1b[36m[--]\x1b[0m {msg}");
}

/// Print a warning message
pub fn warn(msg: &str) {
    eprintln!("\x1b[33m[!!]\x1b[0m {msg}");
}

/// Print an error message
#[allow(dead_code)]
pub fn error(msg: &str) {
    eprintln!("\x1b[31m[!!]\x1b[0m {msg}");
}
