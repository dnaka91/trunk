//! Download management for external application. Locate and automatically downloading applications
//! (if needed) to use them in the build pipeline.

use std::{
    ffi::OsStr,
    fs::File,
    io::Read,
    path::{Path, PathBuf},
};

use anyhow::{anyhow, bail, ensure, Context, Result};
use async_process::Command;
use async_std::{fs::File as AsyncFile, io};
use directories_next::ProjectDirs;
use flate2::read::GzDecoder;
use surf::middleware::Redirect;
use tar::Archive as TarArchive;

/// The application to locate and eventually download when calling [`binary`].
#[derive(Clone, Copy, Debug)]
pub enum Application {
    /// wasm-bindgen for generating the JS bindings.
    WasmBindgen,
    /// wasm-opt to improve performance and size of the output file further.
    WasmOpt,
}

impl Application {
    /// Base name of the executable without extension.
    fn name(&self) -> &str {
        match self {
            Self::WasmBindgen => "wasm-bindgen",
            Self::WasmOpt => "wasm-opt",
        }
    }

    /// Path of the executable within the downloaded archive.
    fn path(&self) -> &str {
        if cfg!(windows) {
            match self {
                Self::WasmBindgen => "wasm-bindgen.exe",
                Self::WasmOpt => "bin/wasm-opt.exe",
            }
        } else {
            match self {
                Self::WasmBindgen => self.name(),
                Self::WasmOpt => "bin/wasm-opt",
            }
        }
    }

    /// Default version to use if not set by the user.
    fn default_version(&self) -> &str {
        match self {
            Self::WasmBindgen => "0.2.73",
            Self::WasmOpt => "version_101",
        }
    }

    /// Target for the current OS as part of the download URL. Can fail as there might be no release
    /// for the current platform.
    fn target(&self) -> Result<&str> {
        Ok(match self {
            Self::WasmBindgen => {
                if cfg!(target_os = "windows") {
                    "pc-windows-msvc"
                } else if cfg!(target_os = "macos") {
                    "apple-darwin"
                } else if cfg!(target_os = "linux") {
                    "unknown-linux-musl"
                } else {
                    bail!("unsupported OS")
                }
            }
            Self::WasmOpt => {
                if cfg!(target_os = "windows") {
                    "windows"
                } else if cfg!(target_os = "macos") {
                    "macos"
                } else if cfg!(target_os = "linux") {
                    "linux"
                } else {
                    bail!("unsupported OS")
                }
            }
        })
    }

    /// Direct URL to the release of an application for download.
    fn url(&self, version: &str) -> Result<String> {
        Ok(match self {
            Self::WasmBindgen => format!(
                "https://github.com/rustwasm/wasm-bindgen/releases/download/{version}/wasm-bindgen-{version}-x86_64-{target}.tar.gz",
                version = version,
                target = self.target()?
            ),
            Self::WasmOpt => format!(
                "https://github.com/WebAssembly/binaryen/releases/download/{version}/binaryen-{version}-x86_64-{target}.tar.gz",
                version = version,
                target = self.target()?,
            ),
        })
    }
}

/// Locate the given application and download it if missing.
#[tracing::instrument(level = "trace")]
pub async fn get(app: Application, version: Option<&str>) -> Result<PathBuf> {
    let version = version.unwrap_or_else(|| app.default_version());

    if let Ok(path) = find_system(app, version).await {
        tracing::info!(app = app.name(), version = version, "using system installed binary");
        return Ok(path);
    }

    let cache_dir = cache_dir()?;
    let app_dir = cache_dir.join(format!("{}-{}", app.name(), version));
    let bin_path = app_dir.join(app.path());

    if !is_executable(&bin_path)? {
        let path = download(app, version).await.context("failed downloading release archive")?;
        let file = File::open(&path).context("failed opening downloaded file")?;
        install(app, &file, &app_dir)?;

        drop(file);
        std::fs::remove_file(path).context("failed deleting temporary archive")?;
    }

    Ok(bin_path)
}

/// Try to find a globally system installed version of the application and ensure it is the needed
/// release version.
#[tracing::instrument(level = "trace")]
async fn find_system(app: Application, version: &str) -> Result<PathBuf> {
    let path = which::which(app.name())?;
    let output = Command::new(&path).arg("--version").output().await?;

    ensure!(output.status.success(), "running command failed");

    let text = String::from_utf8_lossy(&output.stdout);
    let text = text.trim();

    let system_version = match app {
        Application::WasmBindgen => text.splitn(2, ' ').nth(1).context("missing version")?.to_owned(),
        Application::WasmOpt => text.splitn(2, ' ').nth(1).context("missing version")?.replace(' ', "_"),
    };

    if system_version == version {
        Ok(path)
    } else {
        bail!("not found")
    }
}

/// Check whether a given path exists, is a file and marked as executable (unix only).
fn is_executable(path: &Path) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }

    let metadata = path.metadata().with_context(|| anyhow!("failed getting metadata of {:?}", path))?;
    if !metadata.is_file() {
        return Ok(false);
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o100 == 0 {
            return Ok(false);
        }
    }

    Ok(true)
}

/// Download a file from its remote location in the given version, extract it and make it ready for
// execution at the given location.
#[tracing::instrument(level = "trace")]
async fn download(app: Application, version: &str) -> Result<PathBuf> {
    tracing::info!(version = version, "downloading {}", app.name());

    let cache_dir = cache_dir().context("failed getting the cache directory")?;
    let client = surf::client().with(Redirect::new(5));

    let mut resp = client
        .get(app.url(version)?)
        .send()
        .await
        .map_err(|e| e.into_inner())
        .context("error sending HTTP request")?;
    ensure!(
        resp.status().is_success(),
        "error downloading binary file: {:?}\n{}",
        resp.status(),
        app.url(version)?
    );

    let temp_out = cache_dir.join(format!("{}-{}.tmp", app.name(), version));
    let mut file = AsyncFile::create(&temp_out).await.context("failed creating temporary output file")?;

    io::copy(resp.take_body().into_reader(), &mut file)
        .await
        .context("failed downloading the archive")?;

    Ok(temp_out)
}

/// Install an application from a downloaded archive locating and copying it to the given target
/// location.
#[tracing::instrument(level = "trace")]
fn install(app: Application, file: &File, target: &Path) -> Result<()> {
    tracing::info!("installing {}", app.name());

    let name = match app {
        Application::WasmBindgen => OsStr::new(if cfg!(windows) { "wasm-bindgen.exe" } else { "wasm-bindgen" }),
        Application::WasmOpt => OsStr::new(if cfg!(windows) { "bin/wasm-opt.exe" } else { "bin/wasm-opt" }),
    };

    let mut archive = TarArchive::new(GzDecoder::new(file));
    let mut file = find_tar_entry(&mut archive, name)?.context("file not found in archive")?;
    let out = target.join(name);

    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent).context("failed creating output directory")?;
    }

    let mut out = File::create(target.join(name)).context("failed creating output file")?;
    std::io::copy(&mut file, &mut out).context("failed copying over final output file from archive")?;

    set_executable_flag(&mut out)?;

    Ok(())
}

/// Locate the cache dir for trunk and make sure it exists.
fn cache_dir() -> Result<PathBuf> {
    let path = ProjectDirs::from("dev", "trunkrs", "trunk")
        .context("failed finding project directory")?
        .cache_dir()
        .to_owned();
    std::fs::create_dir_all(&path).context("failed creating cache directory")?;

    Ok(path)
}

/// Set the executable flag for a file. Only has an effect on UNIX platforms.
fn set_executable_flag(file: &mut File) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mut perms = file.metadata().context("failed getting metadata")?.permissions();
        perms.set_mode(perms.mode() | 0o100);
        file.set_permissions(perms).context("failed setting the executable flag")?;
    }

    Ok(())
}

/// Find an entry in a TAR archive by name and open it for reading. The first part of the path is
/// dropped as that's usually the folder name it was created from.
fn find_tar_entry<R: Read>(archive: &mut TarArchive<R>, path: impl AsRef<Path>) -> Result<Option<impl Read + '_>> {
    for entry in archive.entries().context("failed getting archive entries")? {
        let entry = entry.context("error while getting archive entry")?;
        let name = entry.path().context("invalid entry path")?;

        let mut name = name.components();
        name.next();

        if name.as_path() == path.as_ref() {
            return Ok(Some(entry));
        }
    }

    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[async_std::test]
    async fn download_and_install_binaries() {
        let dir = tempfile::tempdir().unwrap();

        for &app in &[Application::WasmBindgen, Application::WasmOpt] {
            let path = download(app, app.default_version()).await.unwrap();
            install(app, &File::open(&path).unwrap(), dir.path()).unwrap();
            std::fs::remove_file(path).unwrap();
        }
    }
}
