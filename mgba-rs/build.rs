use cc::Build;

fn main() {
    let mut interface_builder = Build::new();
    interface_builder.include("mgba/include");
    interface_builder.cpp(true);
    interface_builder.std("c++20");
    interface_builder.file("interface.cpp");
    interface_builder.warnings(false);
    // See melonds-rs/build.rs: skip cc-rs's automatic `-lstdc++` so it doesn't collide with the
    // static libstdc++.a the frame server links explicitly.
    interface_builder.cpp_set_stdlib(None);
    interface_builder.compile("mgba-rs-interface");

    println!("cargo::rerun-if-changed=interface.cpp");
}
