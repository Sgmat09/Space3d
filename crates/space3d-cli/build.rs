//! Rebuild when anything under `templates/` changes (`include_dir!` alone does not tell cargo
//! about new or edited files on stable Rust).

fn main() {
    println!("cargo:rerun-if-changed=templates");
}
