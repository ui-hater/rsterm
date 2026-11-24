//! Shell integration: make zsh and bash mark where each prompt starts
//! (OSC 133;A), which is what jump-to-prompt navigates by.
//!
//! Like Ghostty, this works without touching the user's dotfiles:
//! - zsh: `ZDOTDIR` points at our directory, whose `.zshenv` restores the
//!   real `ZDOTDIR`, sources the user's `.zshenv` and adds a precmd hook.
//!   zsh then reads `.zshrc` etc. from the restored directory as usual.
//! - bash: `--rcfile` points at a file that sources `~/.bashrc` and appends
//!   to `PROMPT_COMMAND`.
//!
//! fish 4+ emits the marks itself. Other shells are started unchanged.

use std::path::{Path, PathBuf};
use std::process::Command;

const ZSHENV: &str = r#"# rsterm shell integration (generated; changes are overwritten).
if [[ -n "${RSTERM_ORIG_ZDOTDIR+x}" ]]; then
  ZDOTDIR="$RSTERM_ORIG_ZDOTDIR"
  unset RSTERM_ORIG_ZDOTDIR
else
  unset ZDOTDIR
fi
{
  _rsterm_file="${ZDOTDIR-$HOME}/.zshenv"
  [[ ! -r "$_rsterm_file" ]] || builtin source -- "$_rsterm_file"
} always {
  builtin unset _rsterm_file
}
if [[ -o interactive ]]; then
  _rsterm_precmd() { builtin print -n '\e]133;A\a' }
  builtin autoload -Uz add-zsh-hook
  add-zsh-hook precmd _rsterm_precmd
fi
"#;

const BASHRC: &str = r#"# rsterm shell integration (generated; changes are overwritten).
if [[ -f ~/.bashrc ]]; then builtin source ~/.bashrc; fi
_rsterm_prompt() { builtin printf '\e]133;A\a'; }
PROMPT_COMMAND="${PROMPT_COMMAND}"$'\n'"_rsterm_prompt"
"#;

fn dir() -> PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute() && p.is_dir())
        .unwrap_or_else(|| std::env::temp_dir().join(format!("rsterm-{}", unsafe { libc::getuid() })));
    base.join("rsterm-shell-integration")
}

/// Write `text` to `path` unless it already holds exactly that.
fn ensure(path: &Path, text: &str) -> std::io::Result<()> {
    if std::fs::read_to_string(path).is_ok_and(|t| t == text) {
        return Ok(());
    }
    use std::os::unix::fs::DirBuilderExt;
    if let Some(d) = path.parent() {
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(d)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(tmp, path)
}

/// Adjust `cmd` (an interactive `shell` with no arguments yet) to load the
/// integration. Leaves it unchanged for unknown shells or on any error.
pub fn apply(cmd: &mut Command, shell: &str) {
    let name = Path::new(shell).file_name().and_then(|n| n.to_str()).unwrap_or("");
    let d = dir();
    match name {
        "zsh" => {
            let zdir = d.join("zsh");
            if ensure(&zdir.join(".zshenv"), ZSHENV).is_err() {
                return;
            }
            if let Some(orig) = std::env::var_os("ZDOTDIR") {
                cmd.env("RSTERM_ORIG_ZDOTDIR", orig);
            }
            cmd.env("ZDOTDIR", zdir);
        }
        "bash" => {
            let rc = d.join("bash").join("rsterm.bash");
            if ensure(&rc, BASHRC).is_err() {
                return;
            }
            cmd.arg("--rcfile").arg(rc);
        }
        _ => {}
    }
}
