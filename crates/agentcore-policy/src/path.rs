use std::path::{Component, Path, PathBuf};

use agentcore_core::Action;

/// Lexically normalise `path` against `workspace`: relative paths are joined
/// onto the workspace and `.`/`..` segments are resolved without touching the
/// filesystem. The result is always absolute and contains no `..`.
///
/// This defeats `../` tricks in policy matching. It does not resolve symlinks;
/// sandbox backends must still confine file access themselves.
pub fn normalize_path(workspace: &str, path: &str) -> String {
    let joined = if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        Path::new(workspace).join(path)
    };
    let mut out = PathBuf::from("/");
    for component in joined.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(part) => out.push(part),
            Component::RootDir | Component::CurDir | Component::Prefix(_) => {}
        }
    }
    out.to_string_lossy().into_owned()
}

/// Return a copy of `action` with every path normalised (see [`normalize_path`]).
pub fn normalize_action(action: &Action, workspace: &str) -> Action {
    match action {
        Action::FileRead { path } => Action::FileRead {
            path: normalize_path(workspace, path),
        },
        Action::FileWrite { path, bytes } => Action::FileWrite {
            path: normalize_path(workspace, path),
            bytes: *bytes,
        },
        Action::Exec { command, args, cwd } => Action::Exec {
            command: command.clone(),
            args: args.clone(),
            cwd: Some(normalize_path(workspace, cwd.as_deref().unwrap_or("."))),
        },
        Action::Network { host, port } => Action::Network {
            host: host.trim_end_matches('.').to_ascii_lowercase(),
            port: *port,
        },
        other => other.clone(),
    }
}
