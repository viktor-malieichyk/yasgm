//! Command-line argument helpers and output formatting shared by the commands.




pub(crate) fn human_bytes(bytes: u64) -> String {
    match bytes {
        b if b >= 1_073_741_824 => format!("{:.1} GB", b as f64 / 1_073_741_824.0),
        b if b >= 1_048_576 => format!("{:.1} MB", b as f64 / 1_048_576.0),
        b if b >= 1024 => format!("{:.1} KB", b as f64 / 1024.0),
        b => format!("{b} B"),
    }
}

pub(crate) const VALUED_FLAGS: [&str; 7] = ["--version", "--mode", "--keep", "--app", "--settle", "--account", "--to"];

/// Arguments that aren't options or option values. After `--` everything
/// is positional (e.g. a version id that starts with dashes).
pub(crate) fn positionals(args: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 1; // skip the command itself
    while i < args.len() {
        let arg = &args[i];
        if arg == "--" {
            out.extend(args[i + 1..].iter().cloned());
            break;
        }
        if VALUED_FLAGS.contains(&arg.as_str()) {
            i += 2;
            continue;
        }
        if arg.starts_with("--") {
            i += 1;
            continue;
        }
        out.push(arg.clone());
        i += 1;
    }
    out
}

pub(crate) fn parse_app_id(args: &[String]) -> Option<u64> {
    positionals(args).first().and_then(|a| a.parse().ok())
}

pub(crate) fn flag(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
}

pub(crate) fn flag_value(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

// ---- doctor ---------------------------------------------------------------
