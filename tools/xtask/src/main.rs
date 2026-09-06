use std::{
    env,
    path::PathBuf,
    process::{Command, ExitStatus},
};

fn main() {
    let command = env::args().nth(1).unwrap_or_else(|| "verify".to_string());

    match command.as_str() {
        "verify" => verify(),
        other => {
            eprintln!("unknown xtask command: {other}");
            std::process::exit(2);
        }
    }
}

fn verify() {
    let repo_root = repo_root();

    let steps: &[(&str, &[&str])] = &[
        ("cargo fmt --all", &["fmt", "--all"]),
        ("cargo check", &["check"]),
        (
            "cargo clippy --all-targets --all-features",
            &["clippy", "--all-targets", "--all-features"],
        ),
        ("cargo test --all", &["test", "--all"]),
        ("cargo deny check", &["deny", "check"]),
    ];

    for (name, args) in steps {
        println!();
        println!("==> {name}");

        let status = Command::new("cargo")
            .args(*args)
            .current_dir(&repo_root)
            .status()
            .unwrap_or_else(|error| {
                eprintln!("failed to start {name}: {error}");
                std::process::exit(1);
            });

        require_success(name, status);
    }

    println!();
    println!("========================================");
    println!("SHOCS-LC verification passed. 😎");
    println!("========================================");
}

fn repo_root() -> PathBuf {
    // tools/xtask -> tools -> repository root
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|path| path.parent())
        .expect("xtask must live at tools/xtask")
        .to_path_buf()
}

fn require_success(name: &str, status: ExitStatus) {
    if status.success() {
        return;
    }

    let code = status.code().unwrap_or(1);
    eprintln!();
    eprintln!("{name} failed with exit code {code}.");
    std::process::exit(code);
}
