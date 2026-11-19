//! Canonical command and path defaults shared by config profiles and runtime policy.

/// Default allowed commands for Unix platforms.
pub fn default_allowed_commands() -> Vec<String> {
    #[allow(unused_mut)]
    let mut cmds = vec![
        "git".into(),
        "npm".into(),
        "cargo".into(),
        "ls".into(),
        "cat".into(),
        "grep".into(),
        "find".into(),
        "echo".into(),
        "pwd".into(),
        "wc".into(),
        "head".into(),
        "tail".into(),
        "date".into(),
        "df".into(),
        "du".into(),
        "uname".into(),
        "uptime".into(),
        "hostname".into(),
    ];
    // `free` is Linux-only; it does not exist on macOS or other BSDs.
    #[cfg(target_os = "linux")]
    cmds.push("free".into());
    cmds
}

/// Default forbidden paths for Unix platforms.
pub fn default_forbidden_paths() -> Vec<String> {
    vec![
        "/etc".into(),
        "/root".into(),
        "/home".into(),
        "/usr".into(),
        "/bin".into(),
        "/sbin".into(),
        "/lib".into(),
        "/opt".into(),
        "/boot".into(),
        "/dev".into(),
        "/proc".into(),
        "/sys".into(),
        "/var".into(),
        "/tmp".into(),
        "~/.ssh".into(),
        "~/.gnupg".into(),
        "~/.aws".into(),
        "~/.config".into(),
    ]
}
