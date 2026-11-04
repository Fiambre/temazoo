fn main() {
    // Embed the icon and product info in the Windows executable.
    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/temazoo.ico");
        res.set("ProductName", "Temazoo");
        res.set("FileDescription", "Temazoo");
        res.set("OriginalFilename", "temazoo.exe");
        if let Err(e) = res.compile() {
            println!("cargo:warning=no se pudo incrustar el icono: {e}");
        }
    }
}
