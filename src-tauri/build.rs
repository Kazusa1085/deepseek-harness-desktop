//! Build script.
//!
//! Besides the usual Tauri codegen, this publishes the harness version the shell
//! is pinned to and — when the `portable` feature is on — packs the payload: the
//! harness runtime and a Node interpreter, in a single compressed blob the
//! binary embeds and unpacks on first run.

fn main() {
    pinned_harness_version();

    #[cfg(feature = "portable")]
    portable::pack();

    tauri_build::build()
}

/// Publish the harness version the runtime is pinned to, read from the one file
/// that declares it.
///
/// `backend/package.json` is the single source of truth: the bundled runtime is
/// installed from its lockfile, and the `npx` fallback at the end of backend
/// resolution runs `@deepseek-ai/dsh@<that version>` instead of `@latest`. The
/// difference is not cosmetic — a harness release can change the session format
/// generation, and every stored session then has to be migrated, so the shell
/// must not adopt a new one by accident.
fn pinned_harness_version() {
    const MANIFEST: &str = "../backend/package.json";
    println!("cargo:rerun-if-changed={MANIFEST}");

    let text = std::fs::read_to_string(MANIFEST)
        .unwrap_or_else(|error| panic!("cannot read {MANIFEST}: {error}"));
    let version = text
        .split("\"@deepseek-ai/dsh\"")
        .nth(1)
        .and_then(|rest| rest.split('"').nth(1))
        .unwrap_or_else(|| panic!("{MANIFEST} does not name a version for @deepseek-ai/dsh"));
    assert!(
        !version.is_empty() && !version.starts_with(['^', '~', '>', '<', '*']),
        "{MANIFEST} must pin @deepseek-ai/dsh exactly, got {version:?}"
    );

    println!("cargo:rustc-env=DSH_HARNESS_VERSION={version}");
}

#[cfg(feature = "portable")]
mod portable {
    use std::{
        env,
        fs::{self, File},
        io::{self, BufReader, Write},
        path::{Path, PathBuf},
    };

    use flate2::{write::DeflateEncoder, Compression};

    /// Everything that goes into the payload, as `(source, path inside the
    /// unpacked runtime)`.
    fn sources(root: &Path) -> Vec<(PathBuf, PathBuf)> {
        let mut entries = Vec::new();

        // The Node interpreter, fetched by CI before the build.
        let node = root.join("vendor").join(node_file_name());
        if node.is_file() {
            entries.push((node, PathBuf::from(node_file_name())));
        } else {
            println!(
                "cargo:warning=vendor/{} is missing; the portable build will fall back to the \
                 system Node",
                node_file_name()
            );
        }

        // The harness runtime, installed by `npm run backend:install`.
        let modules = root
            .join("..")
            .join("backend")
            .join("node_modules")
            .canonicalize()
            .unwrap_or_else(|_| {
                panic!(
                    "the portable build needs backend/node_modules; run `npm run backend:install`"
                )
            });
        collect(&modules, Path::new("backend/node_modules"), &mut entries);

        entries
    }

    const fn node_file_name() -> &'static str {
        if cfg!(windows) {
            "node.exe"
        } else {
            "node"
        }
    }

    /// Recursively list files under `dir`, recording each one's path relative
    /// to `prefix` in the unpacked layout.
    fn collect(dir: &Path, prefix: &Path, out: &mut Vec<(PathBuf, PathBuf)>) {
        let Ok(read) = fs::read_dir(dir) else { return };
        for entry in read.flatten() {
            let path = entry.path();
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let relative = prefix.join(entry.file_name());
            // Symlinks are skipped: npm creates them for `.bin` shims, and
            // following them would duplicate megabytes into the payload.
            if kind.is_dir() {
                collect(&path, &relative, out);
            } else if kind.is_file() {
                out.push((path, relative));
            }
        }
    }

    /// Write one record: path length, path, unix mode, data length, data.
    fn write_entry<W: Write>(sink: &mut W, source: &Path, name: &Path) -> io::Result<u64> {
        let text = name.to_string_lossy().replace('\\', "/");
        let bytes = text.as_bytes();
        let metadata = source.metadata()?;
        let size = metadata.len();
        // The mode has to travel with the file: several executables live below
        // the payload root (ripgrep, node-pty's spawn-helper, koffi), and
        // `unpack` used to restore the bit for depth-1 entries only.
        #[cfg(unix)]
        let mode = std::os::unix::fs::PermissionsExt::mode(&metadata.permissions());
        #[cfg(not(unix))]
        let mode = 0u32;

        sink.write_all(&(bytes.len() as u32).to_le_bytes())?;
        sink.write_all(bytes)?;
        sink.write_all(&mode.to_le_bytes())?;
        sink.write_all(&size.to_le_bytes())?;

        let mut file = BufReader::new(File::open(source)?);
        let copied = io::copy(&mut file, sink)?;
        assert_eq!(copied, size, "{} changed size while being packed", text);
        Ok(size)
    }

    pub fn pack() {
        let root = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("manifest dir"));
        let out = PathBuf::from(env::var("OUT_DIR").expect("out dir")).join("portable-payload.bin");

        println!("cargo:rerun-if-changed=vendor");
        println!("cargo:rerun-if-changed=../backend/package.json");

        let entries = sources(&root);
        let mut sink = DeflateEncoder::new(
            File::create(&out).expect("create payload"),
            // Level 6: level 9 costs minutes here for a couple of per cent.
            Compression::new(6),
        );

        let mut total = 0u64;
        for (source, name) in &entries {
            total += write_entry(&mut sink, source, name).unwrap_or_else(|e| {
                panic!("packing {} failed: {e}", source.display());
            });
        }
        sink.finish().expect("finish payload");

        let packed = fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
        println!(
            "cargo:warning=portable payload: {} files, {:.0} MB packed from {:.0} MB",
            entries.len(),
            packed as f64 / 1e6,
            total as f64 / 1e6
        );
    }
}
