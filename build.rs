fn main() {
    // SQLx embeds migrations at compile time; make stable Cargo rebuild when
    // migration SQL changes during local development.
    println!("cargo:rerun-if-changed=migrations");
}
