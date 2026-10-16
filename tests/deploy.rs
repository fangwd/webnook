//! Exercise the deploy hook with stub commands, without certificates or systemd.
//! These fixtures target the checked-in example.com template. Customize the
//! deployed copy; editing the template's lineage also requires updating fixtures.
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::process::Command;

fn run_hook(lineage: &str, active: bool, reload_ok: bool) -> (bool, String, String) {
    let tmp = tempfile::tempdir().unwrap();
    let log = tmp.path().join("commands");
    for (name, script) in [
        ("install", "#!/bin/sh\nprintf 'install %s\\n' \"$*\" >> \"$HOOK_TEST_LOG\"\n"),
        ("systemctl", "#!/bin/sh\nprintf 'systemctl %s\\n' \"$*\" >> \"$HOOK_TEST_LOG\"\ncase \"$1\" in\n  is-active) exit \"$HOOK_TEST_ACTIVE\" ;;\n  reload) exit \"$HOOK_TEST_RELOAD\" ;;\n  *) exit 99 ;;\nesac\n"),
    ] {
        let path = tmp.path().join(name);
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let output = Command::new("/bin/sh")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/deploy/certbot-deploy-hook.sh"
        ))
        .env("PATH", tmp.path())
        .env("RENEWED_LINEAGE", lineage)
        .env("HOOK_TEST_LOG", &log)
        .env("HOOK_TEST_ACTIVE", if active { "0" } else { "3" })
        .env("HOOK_TEST_RELOAD", if reload_ok { "0" } else { "1" })
        .output()
        .unwrap();
    (
        output.status.success(),
        std::fs::read_to_string(log).unwrap_or_default(),
        String::from_utf8(output.stderr).unwrap(),
    )
}

#[test]
fn unrelated_certificate_renewals_do_nothing() {
    for lineage in [
        "/etc/letsencrypt/live/other.example",
        "/etc/letsencrypt/live/example.com/",
    ] {
        let (success, commands, stderr) = run_hook(lineage, true, true);
        assert!(success);
        assert!(commands.is_empty(), "{commands}");
        assert_eq!(
            stderr,
            format!("skipping {lineage} (expected /etc/letsencrypt/live/example.com)\n")
        );
    }
}

#[test]
fn initial_copy_works_before_the_service_is_running() {
    let (success, commands, stderr) = run_hook("/etc/letsencrypt/live/example.com", false, true);
    assert!(success);
    assert!(stderr.is_empty(), "{stderr}");
    assert!(commands.contains("install -m 0644 -o root -g my-site /etc/letsencrypt/live/example.com/fullchain.pem /etc/my-site/tls/fullchain.pem"));
    assert!(commands.contains("install -m 0640 -o root -g my-site /etc/letsencrypt/live/example.com/privkey.pem /etc/my-site/tls/privkey.pem"));
    assert!(commands.ends_with("systemctl is-active --quiet my-site\n"));
}

#[test]
fn matching_certificate_reloads_the_running_site() {
    let (success, commands, stderr) = run_hook("/etc/letsencrypt/live/example.com", true, true);
    assert!(success);
    assert!(stderr.is_empty(), "{stderr}");
    assert!(commands.ends_with("systemctl is-active --quiet my-site\nsystemctl reload my-site\n"));
}

#[test]
fn reload_failure_is_reported() {
    let (success, _, _) = run_hook("/etc/letsencrypt/live/example.com", true, false);
    assert!(!success);
}
