pub mod executor;
pub mod paths;
pub mod shell;

/// Which OS we're running on — detected once at startup, carried everywhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Platform {
    Linux,
    Mac,
    Windows,
}

impl Platform {
    pub fn current() -> Self {
        #[cfg(target_os = "linux")]
        return Platform::Linux;
        #[cfg(target_os = "macos")]
        return Platform::Mac;
        #[cfg(target_os = "windows")]
        return Platform::Windows;
        #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
        compile_error!("Unsupported platform");
    }

    pub fn is_unix(&self) -> bool {
        matches!(self, Platform::Linux | Platform::Mac)
    }
}

/// Abstract step kinds — what a procedure step *means*, not how it runs.
/// The executor layer translates these to platform-native commands.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StepKind {
    /// Copy a file or directory
    FileCopy {
        src: Arg,
        dst: Arg,
    },
    /// Move a file or directory
    FileMove {
        src: Arg,
        dst: Arg,
    },
    /// Delete file(s)
    FileDelete {
        target: Arg,
        recursive: bool,
    },
    /// Create a directory (including parents)
    MkDir {
        path: Arg,
    },
    /// List directory contents, optionally filtered
    ListDir {
        path: Arg,
        pattern: Option<String>,
    },
    /// Read file contents into output capture
    ReadFile {
        path: Arg,
    },
    /// Write string content to a file
    WriteFile {
        path: Arg,
        content: Arg,
        append: bool,
    },
    /// Sample rows from a delimited file
    DataSample {
        file: Arg,
        pct: f32,
        stratified: bool,
        strat_col: Option<String>,
        output: Arg,
    },
    /// Join two delimited files on a key column
    DataJoin {
        left: Arg,
        right: Arg,
        on: String,
        output: Arg,
    },
    /// HTTP call (GET/POST/PUT/PATCH/DELETE)
    HttpCall {
        url: Arg,
        method: HttpMethod,
        body: Option<Arg>,
        headers: Vec<(String, String)>,
        output: Option<Arg>,
    },
    /// Set an environment variable for subsequent steps
    SetEnv {
        key: String,
        value: Arg,
    },
    /// Raw shell command — escape hatch, platform-tagged
    /// Stored per-platform so wraps are portable even with raw steps
    ShellRaw {
        cmd: String,
        platform: Platform,
    },
    /// Print a message to the user
    Echo {
        message: Arg,
    },
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum HttpMethod {
    Get,
    Post,
    Put,
    Patch,
    Delete,
}

/// An argument to a step — either a literal value or a bound slot ($1, $2…)
/// or a reference to the output of a previous step.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Arg {
    /// Literal string, possibly containing shellexpand patterns
    Literal { value: String },
    /// Positional slot from the call site: $1, $2, …
    Slot { index: usize },
    /// Capture output from a previous step by its index
    StepOutput { step_index: usize },
}

impl Arg {
    pub fn literal(s: impl Into<String>) -> Self {
        Arg::Literal { value: s.into() }
    }

    pub fn slot(index: usize) -> Self {
        Arg::Slot { index }
    }

    pub fn step_output(step_index: usize) -> Self {
        Arg::StepOutput { step_index }
    }

    /// Resolve the arg to a concrete string given bound slots and prior outputs.
    pub fn resolve(
        &self,
        slots: &[String],
        outputs: &[Option<String>],
    ) -> anyhow::Result<String> {
        match self {
            Arg::Literal { value } => Ok(expand_env(value)),
            Arg::Slot { index } => slots
                .get(*index)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("Missing slot argument ${}", index + 1)),
            Arg::StepOutput { step_index } => outputs
                .get(*step_index)
                .and_then(|o| o.clone())
                .ok_or_else(|| anyhow::anyhow!("Step {} produced no output", step_index)),
        }
    }
}

fn expand_env(s: &str) -> String {
    // Simple $VAR and ${VAR} expansion without external crate
    let mut result = s.to_string();
    for (k, v) in std::env::vars() {
        result = result.replace(&format!("${{{}}}", k), &v);
        result = result.replace(&format!("${}", k), &v);
    }
    result
}
