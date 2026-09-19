// Creating a VM through Hypervisor.framework needs `com.apple.security.hypervisor`, and
// libkrun `dlopen`s libkrunfw, which needs library validation off. Both are carried by a code
// signature, and `cargo build` produces an unsigned binary — so something has to be signed on
// the way to a boot however this is arranged.
const ENTITLEMENTS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "https://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>com.apple.security.hypervisor</key>
    <true/>
    <key>com.apple.security.cs.disable-library-validation</key>
    <true/>
</dict>
</plist>
"#;

// Signed ad-hoc: the entitlements are what a boot needs, and who signed them is not checked
// on the machine that runs them. Entitlements are read at `exec`, so this has to happen to
// the file before it is started and cannot be done to a process already running.
pub fn sign(binary: &std::path::Path) -> anyhow::Result<()> {
    let plist = binary.with_extension("entitlements.plist");
    std::fs::write(&plist, ENTITLEMENTS)?;

    let signed = std::process::Command::new("codesign")
        .args(["-s", "-", "--force", "--entitlements"])
        .arg(&plist)
        .arg(binary)
        .status();
    let _ = std::fs::remove_file(&plist);

    let signed = signed?;
    anyhow::ensure!(
        signed.success(),
        "codesign of {} failed ({signed}) — without the hypervisor entitlement a boot cannot \
         create a VM",
        binary.display()
    );
    Ok(())
}
