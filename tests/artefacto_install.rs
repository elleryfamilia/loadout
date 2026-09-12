//! The consent-gated install's one moving part: running artefacto's
//! installer script. The download is stood in for by a local script named
//! through `LOADOUT_ARTEFACTO_INSTALLER`, so nothing here touches the
//! network. One test in this binary, because it sets an environment
//! variable and a second thread would race it.

use std::os::unix::fs::PermissionsExt;

#[test]
fn install_runs_the_installer_script_and_reports_its_failure() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("installed");
    let script = dir.path().join("installer.sh");
    std::fs::write(
        &script,
        format!("#!/bin/sh\ntouch '{}'\n", marker.display()),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::env::set_var(loadout::artefacto::INSTALLER_ENV, &script);

    loadout::artefacto::install().expect("the script ran");
    assert!(marker.exists(), "the installer ran through sh");

    std::fs::write(&script, "#!/bin/sh\nexit 3\n").unwrap();
    let err = loadout::artefacto::install().expect_err("a failing installer is an error");
    assert!(format!("{err:#}").contains("exited with"), "{err:#}");
    std::env::remove_var(loadout::artefacto::INSTALLER_ENV);
}
