//! From what the player picked (their Pure disc image or its `default.xex`) to a game ready to
//! start: the disc's files copied out of an image, the executable checked, and the game library
//! built (src/builder/).

use std::fs;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::guest::EXPECTED_XEX_SHA256;
use crate::progress::Progress;
use crate::{builder, data_dir, xiso};

const XEX_NAME: &str = "default.xex";
const ISO_EXTENSION: &str = "iso";
const DISC_DIRECTORY: &str = "disc";
/// Holds the path of the last game that started, which the home screen offers next time.
const LAST_GAME_FILE: &str = "last-game";

/// A game ready to start.
pub struct Prepared {
    pub xex: PathBuf,
    pub library: PathBuf,
}

pub fn prepare(source: &Path, progress: &Progress) -> Result<Prepared, String> {
    let is_image = source
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case(ISO_EXTENSION));
    let (xex, sha256) = if is_image {
        from_image(source, progress)?
    } else {
        progress.stage("Checking your copy of the game");
        let bytes = fs::read(source).map_err(|error| format!("{}: {error}", source.display()))?;
        (source.to_path_buf(), checked_sha256(&bytes, source)?)
    };
    let library = builder::ensure(&xex, &sha256, progress)?;
    // What the player chose (their disc image, not the copy made from it), as an absolute path,
    // so the home screen offers it whatever directory the runtime is started from. Not being able
    // to remember it only means choosing again next time.
    let _ = std::path::absolute(source).and_then(|chosen| {
        fs::create_dir_all(data_dir::root())?;
        fs::write(
            data_dir::root().join(LAST_GAME_FILE),
            chosen.to_string_lossy().as_bytes(),
        )
    });
    Ok(Prepared { xex, library })
}

/// The disc image or executable that started last time, if it is still there.
pub fn remembered() -> Option<PathBuf> {
    let path = PathBuf::from(fs::read_to_string(data_dir::root().join(LAST_GAME_FILE)).ok()?);
    path.is_file().then_some(path)
}

/// The executable's SHA-256, if it is the release the runtime supports.
fn checked_sha256(xex: &[u8], source: &Path) -> Result<String, String> {
    let sha256 = format!("{:x}", Sha256::digest(xex));
    if sha256 != EXPECTED_XEX_SHA256 {
        return Err(format!(
            "{} is not the copy of Pure this runtime supports: its executable's SHA-256 is {sha256}, \
             and the runtime needs the World release with no title update ({EXPECTED_XEX_SHA256}).",
            source.display()
        ));
    }
    Ok(sha256)
}

/// Copies the disc's files out of the image once (checking the executable first, so the wrong
/// disc fails before gigabytes are copied), and returns the copied executable.
fn from_image(image_path: &Path, progress: &Progress) -> Result<(PathBuf, String), String> {
    let io = |error: std::io::Error| format!("{}: {error}", image_path.display());
    progress.stage("Reading your disc image");
    // Its error already names the file.
    let mut image = xiso::Image::open(image_path).map_err(|error| error.to_string())?;
    let entries = image.entries().map_err(io)?;
    let xex_entry = entries
        .iter()
        .find(|entry| entry.path.as_os_str().eq_ignore_ascii_case(XEX_NAME))
        .ok_or_else(|| format!("{} has no {XEX_NAME}", image_path.display()))?;
    let sha256 = checked_sha256(&image.read(xex_entry).map_err(io)?, image_path)?;
    let disc = data_dir::game(&sha256).join(DISC_DIRECTORY);
    let xex = disc.join(&xex_entry.path);
    if !xex.is_file() {
        progress.stage("Copying the game's files from your disc image (a few minutes)");
        fs::create_dir_all(disc.parent().unwrap_or(&disc))
            .map_err(|error| format!("{}: {error}", disc.display()))?;
        image
            .extract(&entries, &disc, |done, total| progress.count(done, total))
            .map_err(io)?;
    }
    Ok((xex, sha256))
}
