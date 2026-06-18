use std::{fs, process::Command};

use tempfile::tempdir;

fn demons() -> Command {
    Command::new(env!("CARGO_BIN_EXE_demons"))
}

#[test]
fn prints_help_and_version() {
    let help = demons().arg("--help").output().unwrap();
    assert!(help.status.success());
    assert!(String::from_utf8_lossy(&help.stdout).contains("development commands side-by-side"));

    let version = demons().arg("--version").output().unwrap();
    assert!(version.status.success());
    assert_eq!(
        String::from_utf8_lossy(&version.stdout).trim(),
        format!("demons {}", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn validates_config_before_requiring_a_tty() {
    let temp = tempdir().unwrap();
    let path = temp.path().join("demons.toml");
    fs::write(
        &path,
        r#"
            [[task]]
            name = "web"
            command = "npm run dev"
            cwd = "missing"
        "#,
    )
    .unwrap();

    let output = demons()
        .args(["--config", path.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("is not a directory"));
}

#[test]
fn reports_missing_config_without_prompting_on_piped_input() {
    let temp = tempdir().unwrap();
    let output = demons().current_dir(temp.path()).output().unwrap();

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("no demons.toml found"));
}

#[test]
fn init_requires_an_interactive_terminal() {
    let temp = tempdir().unwrap();
    let output = demons()
        .arg("init")
        .current_dir(temp.path())
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("demons init requires an interactive terminal")
    );
}
