fn main() {
    // Recompile when registry files change — include_dir! embeds them at
    // compile time but cargo doesn't track them automatically.
    println!("cargo::rerun-if-changed=../../registry");
}
