//! Dedicated headless server: `cargo run --bin server -- [--port 7777]`.

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    use space3d::prelude::*;
    let args: Vec<String> = std::env::args().skip(1).collect();
    let port = {{crate_name}}::arg_value(&args, "--port")
        .and_then(|p| p.parse().ok())
        .unwrap_or({{crate_name}}::DEFAULT_PORT);
    let mut app = App::new();
    {{crate_name}}::build_server(&mut app, port);
    println!("{{title}} server on ws://0.0.0.0:{port} (Ctrl-C to stop)");
    app.run();
}

#[cfg(target_arch = "wasm32")]
fn main() {}
