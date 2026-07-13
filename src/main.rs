use anyhow::Context;
use camino::{Utf8Component, Utf8Path, Utf8PathBuf};
use clap::{Parser, Subcommand};
use flate2::read::GzDecoder;
use once_cell::sync::Lazy;
use sha2::{Digest, Sha256};
use std::fmt::Write as _;
use tar::{Archive, EntryType};
use ureq::BodyReader;

static BASE_DIR: Lazy<Utf8PathBuf> = Lazy::new(|| {
    let home_dir = dirs::home_dir().expect("could not determine home directory");
    let utf8_home_dir = Utf8PathBuf::from_path_buf(home_dir)
        .expect("how tf is your home directory not a utf8 path");
    utf8_home_dir.join(".nvmrs")
});

static VERSIONS_DIR: Lazy<Utf8PathBuf> = Lazy::new(|| BASE_DIR.join("versions"));
static ALIASES_DIR: Lazy<Utf8PathBuf> = Lazy::new(|| BASE_DIR.join("aliases"));
/// Stable symlink that `use_version` repoints at the active version's directory. Users add
/// `$CURRENT_DIR/bin` to `$PATH` once; switching versions never requires touching `$PATH` again.
static CURRENT_DIR: Lazy<Utf8PathBuf> = Lazy::new(|| BASE_DIR.join("current"));

#[inline]
fn normalize_version(version: &str) -> &str {
    version.strip_prefix("v").unwrap_or(version)
}

/// Reads `path`, returning the trimmed, version-normalized contents, or `None` if the file does
/// not exist or is blank. Shared by `.nvmrc` and alias-file resolution.
fn read_trimmed_version(path: &Utf8Path) -> anyhow::Result<Option<String>> {
    if !path.exists() {
        return Ok(None);
    }

    let contents =
        std::fs::read_to_string(path).with_context(|| format!("error reading {path}"))?;
    let trimmed = contents.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }

    Ok(Some(normalize_version(trimmed).to_string()))
}

/// Searches the current directory and its ancestors, in order, for a `.nvmrc` file, the same way
/// most tools resolve config that can live anywhere in a project tree.
fn find_nvmrc() -> anyhow::Result<Option<Utf8PathBuf>> {
    let cwd = Utf8PathBuf::from_path_buf(std::env::current_dir()?)
        .map_err(|path| anyhow::anyhow!("current directory {path:?} is not valid utf-8"))?;

    for dir in cwd.ancestors() {
        let candidate = dir.join(".nvmrc");
        if candidate.exists() {
            return Ok(Some(candidate));
        }
    }

    Ok(None)
}

/// Resolves `name` to a version: if it names an existing alias, that alias's target version is
/// used, otherwise `name` is treated as a version number directly.
fn resolve_alias_or_version(name: &str) -> anyhow::Result<String> {
    if let Some(version) = read_trimmed_version(&ALIASES_DIR.join(name))? {
        return Ok(version);
    }

    Ok(normalize_version(name).to_string())
}

/// Resolves the version to use, in precedence order: the explicit argument (an alias name or a
/// version number), the contents of the nearest `.nvmrc` (searching the current directory and its
/// ancestors), or the `default` alias.
fn resolve_version(explicit: Option<String>) -> anyhow::Result<String> {
    if let Some(name) = explicit {
        return resolve_alias_or_version(&name);
    }

    if let Some(nvmrc_path) = find_nvmrc()?
        && let Some(version) = read_trimmed_version(&nvmrc_path)?
    {
        return Ok(version);
    }

    if let Some(version) = read_trimmed_version(&ALIASES_DIR.join("default"))? {
        return Ok(version);
    }

    anyhow::bail!(
        "no version given, no .nvmrc found in the current directory or its ancestors, and no `default` alias set"
    )
}

/// Points `$CURRENT_DIR` at `target`, replacing any existing symlink atomically via rename so a
/// concurrent reader of `$CURRENT_DIR/bin` never observes a missing or half-written symlink.
fn point_current_at(target: &Utf8Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(&*BASE_DIR)
        .with_context(|| format!("error creating {}", &*BASE_DIR))?;

    let tmp_link = BASE_DIR.join(format!(".current.tmp.{}", std::process::id()));
    let _ = std::fs::remove_file(&tmp_link);
    std::os::unix::fs::symlink(target, &tmp_link)
        .with_context(|| format!("error creating symlink at {tmp_link}"))?;
    std::fs::rename(&tmp_link, &*CURRENT_DIR)
        .with_context(|| format!("error swapping symlink into place at {}", &*CURRENT_DIR))?;

    Ok(())
}

/// Switches the active version by repointing `$CURRENT_DIR` at it (see [`resolve_version`] for how
/// the version is chosen). Errors if the resolved version is not installed.
fn use_version(version: Option<String>) -> anyhow::Result<()> {
    let version = resolve_version(version)?;
    if version.is_empty() {
        anyhow::bail!("resolved version is empty");
    }
    let version_dir = VERSIONS_DIR.join(&version);

    if !version_dir.exists() {
        anyhow::bail!("{version} is not installed; run `nvm install {version}` first");
    }

    point_current_at(&version_dir)?;
    println!("now using {version}");

    let current_bin = CURRENT_DIR.join("bin").into_string();
    let on_path = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).any(|p| p == std::path::Path::new(&current_bin)))
        .unwrap_or(false);
    if !on_path {
        println!(
            "note: {current_bin} is not on your PATH yet; add it to your shell profile once, e.g.\n  export PATH=\"{current_bin}:$PATH\""
        );
    }

    Ok(())
}

/// Wraps a reader and feeds every byte read through it into a running SHA-256, so the tarball can
/// be checksum-verified while it streams instead of being buffered to disk first.
struct HashingReader<R> {
    inner: R,
    hasher: Sha256,
}

impl<R: std::io::Read> std::io::Read for HashingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.hasher.update(&buf[..n]);
        Ok(n)
    }
}

/// Fetches `SHASUMS256.txt` for `version` from nodejs.org and returns the expected checksum for
/// the named tarball.
fn fetch_expected_sha256(version: &str, version_file_tarball: &str) -> anyhow::Result<String> {
    let url = format!("https://nodejs.org/dist/v{version}/SHASUMS256.txt");
    let body = ureq::get(&url)
        .call()
        .with_context(|| format!("error fetching {url}"))?
        .body_mut()
        .read_to_string()
        .with_context(|| format!("error reading body of {url}"))?;

    body.lines()
        .find_map(|line| {
            let mut parts = line.split_whitespace();
            let hash = parts.next()?;
            let name = parts.next()?;
            (name == version_file_tarball).then(|| hash.to_string())
        })
        .with_context(|| format!("no checksum entry for {version_file_tarball} in {url}"))
}

/// Unpacks every archive entry into `output_dir`, stripping the leading `root` directory so the
/// node distribution's `bin/`, `lib/`, etc. land directly in `output_dir`. Guards against tar-slip.
fn extract_flattened(
    archive: &mut Archive<GzDecoder<HashingReader<BodyReader<'_>>>>,
    root: &str,
    output_dir: &Utf8Path,
) -> anyhow::Result<()> {
    for entry_result in archive.entries()? {
        let mut entry = entry_result?;
        let path: Utf8PathBuf = entry.path()?.to_path_buf().try_into()?;

        let Ok(stripped_path) = path.strip_prefix(root) else {
            continue;
        };
        if stripped_path.as_str().is_empty() {
            continue;
        }

        // Refuse entries whose flattened path would escape `output_dir` (tar-slip). The checksum
        // is only verified after extraction, so a corrupt or malicious tarball must not be able to
        // write anywhere outside the destination directory.
        if stripped_path.components().any(|component| {
            matches!(
                component,
                Utf8Component::ParentDir | Utf8Component::RootDir | Utf8Component::Prefix(_)
            )
        }) {
            anyhow::bail!("refusing to extract archive entry with unsafe path: {path}");
        }

        let full_path = output_dir.join(stripped_path);

        if let Some(parent) = full_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        match entry.header().entry_type() {
            EntryType::Regular | EntryType::Link | EntryType::Symlink => {
                entry.unpack(&full_path)?;
            }
            _ => {}
        };
    }

    Ok(())
}

/// Downloads, streams, extracts, and checksum-verifies the tarball into `dest`. Returns an error
/// (leaving cleanup to the caller) if anything fails, including a checksum mismatch.
fn download_verify_extract(
    version: &str,
    version_file: &str,
    version_file_tarball: &str,
    url: &str,
    dest: &Utf8Path,
) -> anyhow::Result<()> {
    let expected_sha256 = fetch_expected_sha256(version, version_file_tarball)?;

    let response = ureq::get(url)
        .call()
        .with_context(|| format!("error fetching {version} from {url}"))?;

    println!("successfully fetched beginning of tarball from {url}");
    let hashing_reader = HashingReader {
        inner: response.into_body().into_reader(),
        hasher: Sha256::new(),
    };

    let gz_decoder = GzDecoder::new(hashing_reader);
    let mut archive = Archive::new(gz_decoder);

    extract_flattened(&mut archive, version_file, dest)?;

    // The tar reader stops at the end-of-archive marker, leaving the tail of the gzip stream
    // unread. Drain it so every downloaded byte flows through HashingReader and into the hash.
    let mut remaining_reader = archive.into_inner().into_inner();
    std::io::copy(&mut remaining_reader, &mut std::io::sink())
        .context("error draining remainder of tarball stream")?;

    let digest = remaining_reader.hasher.finalize();
    let mut actual_sha256 = String::with_capacity(digest.len() * 2);
    for byte in digest {
        write!(actual_sha256, "{byte:02x}").expect("writing to a String is infallible");
    }

    if actual_sha256 != expected_sha256 {
        anyhow::bail!(
            "checksum mismatch for {version_file_tarball}: expected {expected_sha256}, got {actual_sha256}"
        );
    }

    Ok(())
}

/// Downloads and verifies the node distribution for `version` from nodejs.org, installing it into
/// `$VERSIONS_DIR/<version>`. Activates it automatically if no version is active yet.
fn install_version(version: String) -> anyhow::Result<()> {
    let version = normalize_version(version.as_str());
    if version.is_empty() {
        anyhow::bail!("no version given");
    }

    let os_family = match std::env::consts::OS {
        "macos" => "darwin",
        "linux" => "linux",
        value => anyhow::bail!("{value} is not a supported os"),
    };

    let arch = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "x64",
        value => anyhow::bail!("{value} is not a supported architecture"),
    };

    let version_file = format!("node-v{version}-{os_family}-{arch}");
    let version_file_tarball = format!("{version_file}.tar.gz");
    let url = format!("https://nodejs.org/dist/v{version}/{version_file_tarball}");

    let version_dir = VERSIONS_DIR.join(version);

    // Extract into a temporary sibling directory and only rename it into its final location once
    // the download has been fully verified. That way a fetch/extract error (or a killed process)
    // never leaves a partial directory that `use_version` would mistake for a complete install.
    std::fs::create_dir_all(&*VERSIONS_DIR)
        .with_context(|| format!("error creating {}", &*VERSIONS_DIR))?;
    let tmp_dir = VERSIONS_DIR.join(format!(".{version}.tmp.{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp_dir);
    std::fs::create_dir_all(&tmp_dir)
        .with_context(|| format!("error creating {tmp_dir}"))?;

    if let Err(err) = download_verify_extract(
        version,
        &version_file,
        &version_file_tarball,
        &url,
        &tmp_dir,
    ) {
        let _ = std::fs::remove_dir_all(&tmp_dir);
        return Err(err);
    }

    let _ = std::fs::remove_dir_all(&version_dir);
    std::fs::rename(&tmp_dir, &version_dir).with_context(|| {
        format!("error moving verified install {tmp_dir} into place at {version_dir}")
    })?;

    println!("successfully verified SHA 256 checksum against response for {version}");
    println!("successfully installed {version} to {version_dir}");

    if std::fs::symlink_metadata(&*CURRENT_DIR).is_err() {
        point_current_at(&version_dir)?;
        println!("no version was active yet, so now using {version}");
    }

    Ok(())
}

/// Records `name` as an alias for `version` by writing the version into `$ALIASES_DIR/<name>`.
/// Errors if the version is not installed.
fn alias_version(version: String, name: String) -> anyhow::Result<()> {
    let version = normalize_version(&version);
    let version_dir = VERSIONS_DIR.join(version);

    if !version_dir.exists() {
        anyhow::bail!("{version} is not installed; run `nvm install {version}` first");
    }

    std::fs::create_dir_all(&*ALIASES_DIR)
        .with_context(|| format!("error creating {}", &*ALIASES_DIR))?;
    std::fs::write(ALIASES_DIR.join(&name), version)
        .with_context(|| format!("error writing alias {name}"))?;

    println!("aliased {name} -> {version}");
    Ok(())
}

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Uses, in this precedence order, the `version` specified by the argument, or the version
    /// specified by `.nvmrc`, or the version specified by the `default` alias.
    Use {
        /// The optional version or alias name to use.  If an alias by this name exists, its
        /// target version is used; otherwise this is treated as a version number directly.  If
        /// not provided, will attempt to read from `.nvmrc` or the `default` alias, or error.
        version: Option<String>,
    },
    /// Installs a version from <https://nodejs.org/dist/>.
    Install {
        /// The name of a directory at <https://nodejs.org/dist/> that contains a nodejs version.
        version: String,
    },
    /// Aliases a specific version (e.g. "22.12.0") to a friendly name (e.g. "default").  The alias
    /// `default` is special, in the sense that nvmrs will use that version as a default when no
    /// other version is provided by `nvm use`.
    Alias { version: String, name: String },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Use { version } => use_version(version),
        Commands::Install { version } => install_version(version),
        Commands::Alias { version, name } => alias_version(version, name),
    }
}
