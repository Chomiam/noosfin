fn main() {
    slint_build::compile("ui/app.slint").expect("compilation Slint");

    // Définitions de trackers embarquées (format Cardigann, reprises de Prowlarr).
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/indexers");
    println!("cargo:rerun-if-changed={}", dir.display());
    let mut files: Vec<_> = std::fs::read_dir(&dir).unwrap().filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "yml")).collect();
    files.sort();
    let mut out = String::from("&[\n");
    for f in files {
        out.push_str(&format!("    ({:?}, include_str!({:?})),\n", f.file_name().unwrap().to_string_lossy(), f.display().to_string()));
    }
    out.push_str("]\n");
    std::fs::write(std::path::Path::new(&std::env::var("OUT_DIR").unwrap()).join("indexers.rs"), out).unwrap();
}
