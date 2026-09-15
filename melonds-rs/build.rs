use cc::Build;

fn main() {
    let mut interface_builder = Build::new();
    interface_builder.cpp(true);
    interface_builder.std("c++20"); // need C++20 for semaphores/threading (melonDS runs way slower without threading)
    interface_builder.file("interface.cpp");
    interface_builder.warnings(false);
    // Consumers (the frame server's build.rs, CMake's Qt link) supply their own C++ runtime
    // explicitly; letting cc-rs add its own `-lstdc++` here pulls in mingw's dynamic
    // libstdc++.dll.a stub ahead of the static archive the binary actually wants, which the
    // linker then rejects as duplicate definitions of the same runtime symbols.
    interface_builder.cpp_set_stdlib(None);
    interface_builder.compile("melonds-rs-interface");

    println!("cargo::rerun-if-changed=interface.cpp");
}
