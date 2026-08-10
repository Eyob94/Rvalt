fn main() {
    let install_dir =
        std::env::var("OCIO_INSTALL_DIR").unwrap_or("/opt/homebrew/opt/opencolorio".into());

    cc::Build::new()
        .cpp(true)
        .std("c++17")
        .include(format!("{install_dir}/include"))
        .compile("ocio");
}
