//! One module per subcommand.

pub mod build;
pub mod ci;
pub mod doctor;
pub mod ios;
pub mod new;
pub mod publish;
pub mod run;
pub mod serve;
pub mod test;

use crate::args::CargoSel;
use crate::process::Cmd;

/// Add `--release`, `-p`, `--bin`, `--features` from `sel` to a cargo command (`bin`
/// overrides `sel.bin` when given).
pub fn cargo_flags(cmd: Cmd, sel: &CargoSel, bin: Option<&str>) -> Cmd {
    cmd.arg_if(sel.release, "--release")
        .opt("-p", sel.package.as_deref())
        .opt("--bin", bin.or(sel.bin.as_deref()))
        .opt("--features", sel.features.as_deref())
}

/// `release` / `debug`: the profile directory name inside `target/<triple>/`.
pub fn profile_dir(release: bool) -> &'static str {
    if release { "release" } else { "debug" }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cargo_flags_are_forwarded() {
        let sel = CargoSel {
            release: true,
            package: Some("game".into()),
            bin: Some("x".into()),
            features: Some("editor".into()),
        };
        let cmd = cargo_flags(Cmd::new("cargo").arg("build"), &sel, Some("game"));
        assert_eq!(
            cmd.display(),
            "cargo build --release -p game --bin game --features editor"
        );
        let cmd = cargo_flags(Cmd::new("cargo").arg("run"), &CargoSel::default(), None);
        assert_eq!(cmd.display(), "cargo run");
        assert_eq!(profile_dir(true), "release");
    }
}
