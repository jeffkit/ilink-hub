// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn builtin_profile_type_from_args<I, S>(args: I) -> Option<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut args = args.into_iter();
    match (args.next(), args.next(), args.next()) {
        (Some(cmd), Some(profile_type), None) if cmd.as_ref() == "profile" => {
            Some(profile_type.as_ref().to_string())
        }
        _ => None,
    }
}

fn run_builtin_profile(profile_type: String) {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime for built-in profile");

    if let Err(e) = runtime.block_on(im_agentproc::bridge::builtin::run_builtin_profile(
        &profile_type,
    )) {
        eprintln!("{e:#}");
        std::process::exit(1);
    }
}

fn main() {
    if let Some(profile_type) = builtin_profile_type_from_args(std::env::args().skip(1)) {
        run_builtin_profile(profile_type);
        return;
    }

    ilink_hub_desktop_lib::run()
}

#[cfg(test)]
mod tests {
    use super::builtin_profile_type_from_args;

    #[test]
    fn detects_builtin_profile_cli_mode() {
        assert_eq!(
            builtin_profile_type_from_args(["profile", "claude-code"]).as_deref(),
            Some("claude-code")
        );
    }

    #[test]
    fn ignores_desktop_mode_args() {
        assert!(builtin_profile_type_from_args(["--help"]).is_none());
        assert!(builtin_profile_type_from_args(["profile"]).is_none());
        assert!(builtin_profile_type_from_args(["profile", "claude-code", "--extra"]).is_none());
    }
}
