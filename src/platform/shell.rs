use super::Platform;

/// Detected shell environment — affects how we hook readline and emit evals.
#[derive(Debug, Clone)]
pub struct ShellEnv {
    pub platform: Platform,
    pub shell_kind: ShellKind,
    /// The string we wrap around expanded commands so the parent shell evals them
    pub eval_wrapper: EvalWrapper,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellKind {
    Bash,
    Zsh,
    Fish,
    PowerShell,
    Cmd,
    Unknown(String),
}

/// How `sm resolve` outputs a command for the parent shell to execute.
/// On Unix we print to stdout and the shell function does `eval $(sm resolve ...)`.
/// On PowerShell we output a script block string.
#[derive(Debug, Clone)]
pub enum EvalWrapper {
    /// Print raw command — parent shell function evals it
    UnixEval,
    /// Wrap in Invoke-Expression compatible string
    PowerShellIex,
    /// Not supported — shell must copy-paste
    Unsupported,
}

impl ShellEnv {
    pub fn detect() -> Self {
        let platform = Platform::current();
        let shell_kind = detect_shell_kind();
        let eval_wrapper = match (&platform, &shell_kind) {
            (Platform::Linux | Platform::Mac, ShellKind::Bash | ShellKind::Zsh | ShellKind::Fish) => {
                EvalWrapper::UnixEval
            }
            (Platform::Windows, ShellKind::PowerShell) => EvalWrapper::PowerShellIex,
            _ => EvalWrapper::Unsupported,
        };
        ShellEnv { platform, shell_kind, eval_wrapper }
    }

    /// Returns the shell hook snippet the user should add to their rc file.
    pub fn hook_snippet(&self) -> String {
        match self.shell_kind {
            ShellKind::Bash => r#"
# shellmind hook — add to ~/.bashrc
function sm() {
    local result
    result=$(command sm resolve "$@" 2>/tmp/sm_err)
    if [ $? -eq 0 ] && [ -n "$result" ]; then
        eval "$result"
    else
        command sm "$@"
    fi
}
"#.to_string(),
            ShellKind::Zsh => r#"
# shellmind hook — add to ~/.zshrc
function sm() {
    local result
    result=$(command sm resolve "$@" 2>/tmp/sm_err)
    if [[ $? -eq 0 ]] && [[ -n "$result" ]]; then
        eval "$result"
    else
        command sm "$@"
    fi
}
"#.to_string(),
            ShellKind::Fish => r#"
# shellmind hook — add to ~/.config/fish/config.fish
function sm
    set result (command sm resolve $argv 2>/tmp/sm_err)
    if test $status -eq 0; and test -n "$result"
        eval $result
    else
        command sm $argv
    end
end
"#.to_string(),
            ShellKind::PowerShell => r#"
# shellmind hook — add to $PROFILE
function sm {
    $result = & (Get-Command sm -CommandType Application).Source resolve @args 2>$null
    if ($LASTEXITCODE -eq 0 -and $result) {
        Invoke-Expression $result
    } else {
        & (Get-Command sm -CommandType Application).Source @args
    }
}
"#.to_string(),
            _ => "# Unsupported shell — run `sm resolve <command>` manually".to_string(),
        }
    }
}

fn detect_shell_kind() -> ShellKind {
    // Check $SHELL on Unix, $PSVersionTable on Windows
    if let Ok(shell) = std::env::var("SHELL") {
        let shell = shell.to_lowercase();
        if shell.contains("zsh") { return ShellKind::Zsh; }
        if shell.contains("bash") { return ShellKind::Bash; }
        if shell.contains("fish") { return ShellKind::Fish; }
        return ShellKind::Unknown(shell);
    }
    // Windows: check if running inside PowerShell
    if std::env::var("PSModulePath").is_ok() {
        return ShellKind::PowerShell;
    }
    if std::env::var("COMSPEC").is_ok() {
        return ShellKind::Cmd;
    }
    ShellKind::Unknown("unknown".to_string())
}
