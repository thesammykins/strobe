//! Swift frames (SPEC §12.2): mangled names made readable by the toolchain's own demangler, and the
//! package a Swift source file belongs to.

use std::io::Write;
use std::process::{Command, Stdio};

/// A mangled Swift symbol (`$s…` Swift 5+, `$S…` 4.2, `_T0…` 4.0, `$e…` Embedded Swift), with or
/// without Mach-O's leading underscore.
pub fn mangled(name: &str) -> bool {
    let n = name.strip_prefix('_').unwrap_or(name);
    ["$s", "$S", "$e", "_T0"].iter().any(|p| n.starts_with(p))
}

/// The names in their simplified readable form (`closure #1 in Index.search(_:)`, as `sample` and
/// Instruments print them), in order, from one `swift-demangle` for all of them. It runs when a view
/// first reads a recording, never while measuring. None without a Swift toolchain.
pub fn demangle(names: &[String]) -> Option<Vec<String>> {
    let input = names.iter().map(|n| format!("{n}\n")).collect::<String>();
    let run = |dev: Option<&str>| -> Option<Vec<String>> {
        let mut c = if cfg!(target_os = "macos") {
            let mut c = Command::new("xcrun");
            c.arg("swift-demangle");
            c
        } else {
            Command::new("swift-demangle")
        };
        c.arg("--simplified").stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
        if let Some(d) = dev {
            c.env("DEVELOPER_DIR", d);
        }
        let mut child = c.spawn().ok()?;
        let mut stdin = child.stdin.take()?;
        let text = input.clone();
        // Written from another thread: the output fills its pipe while the input is still going in.
        let writer = std::thread::spawn(move || stdin.write_all(text.as_bytes()));
        let out = child.wait_with_output().ok()?;
        let _ = writer.join();
        let lines: Vec<String> = String::from_utf8_lossy(&out.stdout).lines().map(str::to_string).collect();
        (out.status.success() && lines.len() == names.len()).then_some(lines)
    };
    // An Xcode whose license nobody accepted blocks the xcrun shim; the Command Line Tools still work.
    const CLT: &str = "/Library/Developer/CommandLineTools";
    run(None).or_else(|| if cfg!(target_os = "macos") && std::path::Path::new(CLT).is_dir() { run(Some(CLT)) } else { None })
}

/// The package of a Swift source path: a dependency's checkout (`…/checkouts/<package>/`, in
/// SwiftPM's `.build` and Xcode's `SourcePackages` alike), `swift` for the standard library inlined
/// into a caller. None for the module's own code, which goes by its binary.
pub fn package(file: &str) -> Option<String> {
    if let Some(i) = file.find("/checkouts/") {
        return file[i + 11..].split('/').next().filter(|p| !p.is_empty()).map(str::to_string);
    }
    (file.contains("/stdlib/public/") || file.contains(".swiftinterface") || file.contains("/usr/lib/swift/")).then(|| "swift".into())
}
