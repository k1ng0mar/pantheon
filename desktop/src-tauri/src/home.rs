//! The user's home folder. Separate from the data dir so cloning, skill
//! discovery and bot workspaces agree on one answer.

pub(crate) fn home() -> Option<std::path::PathBuf> {
    let names = if cfg!(windows) {
        ["USERPROFILE", "HOME"]
    } else {
        ["HOME", "USERPROFILE"]
    };
    names.into_iter().find_map(|name| {
        std::env::var_os(name)
            .map(std::path::PathBuf::from)
            .filter(|p| p.is_absolute())
    })
}
