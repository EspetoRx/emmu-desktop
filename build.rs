fn main() {
    tauri_build::build();

    // MesenCore e SDL2 são copiados para $RESOURCE pelo `bundle.resources` do
    // tauri.conf.json. O binário precisa de um rpath até esse diretório para
    // resolver o `MesenCore.dylib`/`.so` (que por sua vez já tem @loader_path
    // ou $ORIGIN para encontrar o SDL2 empacotado).
    #[cfg(target_os = "macos")]
    println!("cargo:rustc-link-arg=-Wl,-rpath,@executable_path/../Resources");

    #[cfg(target_os = "linux")]
    {
        // AppImage: usr/bin + usr/lib; pacotes .deb: /usr/bin + /usr/lib/<id>.
        println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN/../lib");
        println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN/../lib/com.emmu.app");
    }
}
