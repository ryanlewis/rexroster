use std::process::Command;

#[test]
fn skill_prints_the_guide_without_a_rex_session() {
    let out = Command::new(env!("CARGO_BIN_EXE_rexroster"))
        .arg("skill")
        .env_remove("REX_SESSION")
        .env_remove("REXROSTER_LEAD")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.starts_with("# rexroster"));
    assert!(text.contains("rexroster spawn"));
}

#[test]
fn skill_ignores_display_flags_given_before_it() {
    let out = Command::new(env!("CARGO_BIN_EXE_rexroster"))
        .args(["--border", "rounded", "skill"])
        .env_remove("REX_SESSION")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!out.stdout.is_empty());
}
