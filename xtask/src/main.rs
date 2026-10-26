// The bindgen half of the build. Being a workspace member, its
// wasm-bindgen-cli-support shares Cargo.lock with the wasm-bindgen crate, so
// the two can't drift apart the way a separately installed CLI does.
fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let (Some(wasm), Some(out)) = (args.next(), args.next()) else {
        anyhow::bail!("usage: xtask <copd.wasm> <out-dir>");
    };
    wasm_bindgen_cli_support::Bindgen::new()
        .input_path(wasm)
        .web(true)?
        .typescript(false)
        .generate(out)
}
