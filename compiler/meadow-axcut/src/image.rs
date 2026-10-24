//! **A program of AxCut as a file**: what `meadow` hands an engine that runs
//! in a process of its own -- Silo's interpreter, `silo/vm` -- and that reads
//! it back. `docs/RUNTIME-MERGE.md`, phase 5.
//!
//! The program, made linear -- the memory pass (`linear`) done once, by
//! whoever writes the image, rather than by every process that runs it --
//! and how its memory is managed. The file is `postcard`'s
//! encoding of [`Image`], after a line saying what it is and which version,
//! so that a reader built from other sources refuses it rather than
//! misreading it.

use crate::config::Config;
use crate::linear::LBlock;
use crate::{Label, Program};

/// What a file of this kind starts with.
const MAGIC: &[u8] = b"meadow axcut image 4\n";

/// A program to run, and how.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(bound(deserialize = "'de: 'static"))]
pub struct Image {
    pub program: Program,
    /// How it is run: its memory, its stack.
    pub config: Config,
    /// For a program of tests, where each begins: the one run is the one
    /// numbered by the first argument, as a test executable's is. Empty for
    /// a program, which begins at its entry.
    pub tests: Vec<Label>,
    /// `linear::program` of the program, for its entry or its tests: each
    /// definition's index, label and linear block.
    pub linear: Vec<(usize, Label, LBlock)>,
}

/// `image` as the bytes of a file.
pub fn encode(image: &Image) -> Result<Vec<u8>, String> {
    let body = postcard::to_allocvec(image).map_err(|e| format!("encoding a program: {e}"))?;
    let mut out = MAGIC.to_vec();
    out.extend_from_slice(&body);
    Ok(out)
}

/// The image in `bytes`, which live as long as the program does: a message
/// in it is borrowed for the rest of the run.
pub fn decode(bytes: &'static [u8]) -> Result<Image, String> {
    let Some(body) = bytes.strip_prefix(MAGIC) else {
        return Err("not a program of AxCut, or one written by another version".into());
    };
    postcard::from_bytes(body).map_err(|e| format!("reading a program: {e}"))
}
