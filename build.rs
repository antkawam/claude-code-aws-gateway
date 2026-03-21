fn main() {
    // Ensure cargo recompiles when the embedded portal HTML changes.
    println!("cargo::rerun-if-changed=static/index.html");
}
