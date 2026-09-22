use clap::CommandFactory;
use miette::{Context, IntoDiagnostic, Result, miette};
use tracing::instrument;

use crate::cli::CompletionArgs;

/// `gwl-jobs completion` (design doc 0001 §8): shell completions on stdout,
/// for the explicit shell or the one inferred from $SHELL.
#[instrument(skip_all)]
pub fn execute_completion(args: CompletionArgs) -> Result<()> {
    let shell = match &args.shell {
        Some(name) => shell_from_name(name)?,
        None => infer_shell()?,
    };
    let mut cmd = crate::cli::Cli::command();
    // Generate into a buffer, then write once: a consumer closing the pipe
    // early (`gwl-jobs completion bash | head`) is normal Unix usage, and
    // clap_complete unwraps its writes — a broken stdout pipe must exit
    // cleanly, not panic (found live during the Increment 5 smoke test).
    let mut script: Vec<u8> = Vec::new();
    clap_complete::generate(shell, &mut cmd, crate::APP_NAME, &mut script);
    write_completions(std::io::stdout(), &script)
}

/// Write the generated completion script; EPIPE is a clean exit (the
/// consumer closed the pipe), anything else is a real I/O failure.
fn write_completions<W: std::io::Write>(mut out: W, script: &[u8]) -> Result<()> {
    match out.write_all(script) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        Err(err) => Err(err).into_diagnostic(),
    }
}

/// Resolve a shell by name for `gwl-jobs completion`.
fn shell_from_name(name: &str) -> Result<clap_complete::Shell> {
    let lower = name.to_ascii_lowercase();
    let shell = match lower.as_str() {
        "bash" => clap_complete::Shell::Bash,
        "zsh" => clap_complete::Shell::Zsh,
        "fish" => clap_complete::Shell::Fish,
        _ => {
            return Err(miette!(
                "unsupported shell '{lower}' (expected bash, zsh, or fish)"
            ));
        }
    };
    Ok(shell)
}

/// Infer the invoking shell from $SHELL (basename only; paths like
/// /usr/bin/fish are common).
fn infer_shell() -> Result<clap_complete::Shell> {
    let shell = std::env::var_os("SHELL")
        .map(|s| s.to_string_lossy().into_owned())
        .ok_or_else(|| {
            miette!("could not infer the shell from $SHELL; pass one explicitly (bash, zsh, fish)")
        })?;
    let basename = shell.rsplit('/').next().unwrap_or(&shell);
    shell_from_name(basename).wrap_err_with(|| format!("$SHELL is '{shell}'"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_from_name_is_case_insensitive_and_validated() {
        assert!(matches!(
            shell_from_name("bash"),
            Ok(clap_complete::Shell::Bash)
        ));
        assert!(matches!(
            shell_from_name("ZSH"),
            Ok(clap_complete::Shell::Zsh)
        ));
        assert!(matches!(
            shell_from_name("fish"),
            Ok(clap_complete::Shell::Fish)
        ));
        assert!(shell_from_name("powershell").is_err());
    }

    /// A Write whose target is a closed pipe: every write fails with EPIPE.
    struct BrokenPipe;

    impl std::io::Write for BrokenPipe {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn completions_exit_cleanly_on_broken_pipe() {
        // Regression: `gwl-jobs completion bash | head` must not panic
        // when the consumer closes the pipe early (found live in the
        // Increment 5 smoke test).
        assert!(write_completions(BrokenPipe, b"script").is_ok());
        assert!(write_completions(Vec::new(), b"script").is_ok());
    }

    #[test]
    fn completion_generation_produces_a_script() {
        // Smoke: bash generation through the real clap command produces
        // actual completion script content (not an empty buffer). The
        // module-level `use clap::CommandFactory` is in scope here.
        let shell = shell_from_name("bash").unwrap();
        let mut cmd = crate::cli::Cli::command();
        let mut buf: Vec<u8> = Vec::new();
        clap_complete::generate(shell, &mut cmd, crate::APP_NAME, &mut buf);
        let script = String::from_utf8(buf).unwrap();
        assert!(script.contains("gwl-jobs"), "script: {script}");
        assert!(script.contains("complete"));
    }
}
