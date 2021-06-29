#![cfg(not(target_arch = "wasm32"))]

use crate::{Error, Result};

pub struct RustFmtCommand<'cmd> {
    /// either 'rustfmt', (the default, assumes ~/.cargo/bin is in your path,
    /// or a path to an executable
    program: &'cmd str,
    /// rust Edition, default "2018"
    edition: &'cmd str,
    /// any additional args
    extra: Vec<&'cmd str>,
}

impl<'cmd> Default for RustFmtCommand<'cmd> {
    fn default() -> Self {
        RustFmtCommand {
            program: "rustfmt",
            edition: "2018",
            extra: Vec::new(),
        }
    }
}

impl<'cmd> RustFmtCommand<'cmd> {
    pub fn execute(&self, source_files: Vec<std::path::PathBuf>) -> Result<()> {
        if !matches!(self.edition, "2015" | "2018" | "2021") {
            return Err(Error::Rustfmt(format!("invalid edition: {}", self.edition)));
        }
        let missing = source_files
            .iter()
            .filter(|p| !p.is_file())
            .map(|p| p.to_string_lossy().into_owned())
            .collect::<Vec<String>>();
        if !missing.is_empty() {
            return Err(Error::Rustfmt(format!(
                "missing source file(s) '{}'",
                missing.join(",")
            )));
        }
        let source_paths: Vec<std::borrow::Cow<'_, str>> =
            source_files.iter().map(|p| p.to_string_lossy()).collect();

        let mut args = vec!["--edition", self.edition];
        args.extend(self.extra.iter());
        args.extend(source_paths.iter().map(|p| p.as_ref()));
        let mut child = std::process::Command::new(self.program)
            .args(&args)
            .spawn()
            .map_err(|e| Error::Rustfmt(format!("failed to start: {}", e.to_string())))?;

        let code = child.wait().map_err(|e| {
            Error::Rustfmt(format!("failed waiting for rustfmt: {}", e.to_string()))
        })?;
        if !code.success() {
            return Err(Error::Rustfmt(code.to_string()));
        }
        Ok(())
    }
}
