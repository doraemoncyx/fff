// The Binary verdict's name-based tables; see `docs/adr/0002-binary-verdict-from-extension.md`.

use std::path::Path;
use std::sync::OnceLock;

// Formats whose content is never text. Sorted: the lookup is a binary search.
#[rustfmt::skip]
pub const BINARY_EXTENSIONS: &[&str] = &[
    "7z", "a", "aac", "ai", "apk", "appimage", "arrow", "astc",
    "avi", "avif", "bcmap", "bin", "blend", "blp", "bmp", "bz2",
    "cab", "class", "cmi", "cmt", "cmti", "cmx", "cpio", "cr2",
    "crx", "cur", "db", "db-shm", "db-wal", "dds", "deb", "dex",
    "dia", "dll", "dmg", "dng", "doc", "docx", "dylib", "egg",
    "elf", "eot", "exe", "exr", "flac", "flatpak", "flv", "ggml",
    "gguf", "gif", "glb", "gz", "h5", "hdf5", "hdr", "heic",
    "heif", "icns", "ico", "iso", "j2k", "jar", "joblib", "jp2",
    "jpeg", "jpg", "jsonlz4", "jxl", "ktx", "ktx2", "ldb", "lib",
    "lz4", "lzma", "m4a", "mdb", "mhtml", "mkv", "mov", "mp3",
    "mp4", "mpeg", "mpg", "msi", "nef", "nib", "npy", "npz",
    "nupkg", "o", "ogg", "onnx", "opus", "otf", "pak", "parquet",
    "pb", "pcm", "pdf", "png", "ppt", "pptx", "psd", "pt",
    "pvr", "pyc", "pyo", "rar", "reapeaks", "rgbe", "rpm", "safetensors",
    "so", "sqlite", "sqlite-shm", "sqlite-wal", "sqlite3", "sqlite3-shm", "sqlite3-wal", "suo",
    "swiftdeps", "swiftdeps~", "swiftdoc", "swiftmodule", "swiftsourceinfo", "tar", "tflite", "tfrecord",
    "tga", "tif", "tiff", "ttf", "war", "wasm", "wav", "webarchive",
    "webm", "webp", "whl", "wma", "wmv", "woff", "woff2", "xls",
    "xlsx", "xz", "zip", "zst",
];

// Formats whose content is text by specification, so the name settles them.
// `.txt`, `.csv` and `.dat` are absent on purpose: they carry arbitrary
// encodings (UTF-16 has NUL bytes) and stay on the byte path.
#[rustfmt::skip]
pub const TEXT_EXTENSIONS: &[&str] = &[
    "adoc", "bash", "bat", "c", "cc", "cfg", "cjs", "clj",
    "cljs", "cmake", "cmd", "conf", "cpp", "cs", "css", "cxx",
    "dart", "diff", "el", "erl", "ex", "exs", "fish", "fs",
    "fsx", "go", "gradle", "h", "hh", "hpp", "hrl", "hs",
    "htm", "html", "hxx", "ini", "ipynb", "java", "jl", "js",
    "json", "json5", "jsonc", "jsx", "kt", "kts", "less", "lisp",
    "log", "lua", "m", "markdown", "md", "mjs", "ml", "mli",
    "mm", "nim", "org", "patch", "php", "pl", "pm", "properties",
    "proto", "ps1", "psm1", "py", "pyi", "r", "rb", "rkt",
    "rs", "rst", "sass", "sbt", "scala", "scm", "scss", "sh",
    "sql", "svelte", "svg", "swift", "tex", "toml", "ts", "tsx",
    "vim", "vue", "xml", "yaml", "yml", "zig", "zsh",
];

// Extensions that name both a text and a binary format: reading is unavoidable.
const MIXED_USE_EXTENSIONS: &[&str] = &["blp"];

// Operator extensions on top of `BINARY_EXTENSIONS`; set once, at startup.
static EXTRA_BINARY: OnceLock<Box<[String]>> = OnceLock::new();

// What a file's name alone says about its contents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExtensionVerdict {
    Binary,
    Text,
    // Both formats answer to this extension, so only the bytes can decide.
    MixedUse,
    // No opinion: the content pass decides later.
    Unknown,
}

impl ExtensionVerdict {
    // The name already answered, so the verdict costs no read.
    pub(crate) fn settled_by_name(self) -> bool {
        matches!(self, Self::Binary | Self::Text)
    }

    // The extension sits on the binary table, mixed-use included.
    pub(crate) fn names_binary(self) -> bool {
        matches!(self, Self::Binary | Self::MixedUse)
    }
}

// One walk's name-based verdicts: how the two sources split the tree.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct VerdictCounts {
    pub(crate) binary: u64,
    pub(crate) text: u64,
    pub(crate) mixed_use: u64,
    pub(crate) unknown: u64,
}

impl VerdictCounts {
    pub(crate) fn record(&mut self, verdict: ExtensionVerdict) {
        let slot = match verdict {
            ExtensionVerdict::Binary => &mut self.binary,
            ExtensionVerdict::Text => &mut self.text,
            ExtensionVerdict::MixedUse => &mut self.mixed_use,
            ExtensionVerdict::Unknown => &mut self.unknown,
        };
        *slot += 1;
    }
}

// The extension of the last path component, `Path::extension()` semantics:
// a dotfile with no other dot has none.
fn extension_of(name: &str) -> Option<&str> {
    let base = name.rsplit_once(['/', '\\']).map_or(name, |(_, tail)| tail);
    let (stem, ext) = base.rsplit_once('.')?;
    (!stem.is_empty() && !ext.is_empty()).then_some(ext)
}

fn lookup(table: &[&str], ext: &str) -> bool {
    if table.binary_search(&ext).is_ok() {
        return true;
    }
    // The tables are lowercase; a `.PNG` pays one allocation for the retry.
    ext.bytes().any(|b| b.is_ascii_uppercase())
        && table
            .binary_search(&ext.to_ascii_lowercase().as_str())
            .is_ok()
}

// The name's verdict for a file name or a root-relative path.
pub(crate) fn extension_verdict(name: &str) -> ExtensionVerdict {
    extension_verdict_with(name, extra_binary())
}

// As `extension_verdict`, with the extra binary list passed in, so the
// operator-configured additions stay additive and testable.
pub(crate) fn extension_verdict_with(name: &str, extra: &[String]) -> ExtensionVerdict {
    let Some(ext) = extension_of(name) else {
        return ExtensionVerdict::Unknown;
    };
    if lookup(MIXED_USE_EXTENSIONS, ext) {
        return ExtensionVerdict::MixedUse;
    }
    if lookup(BINARY_EXTENSIONS, ext) || extra.iter().any(|e| e.eq_ignore_ascii_case(ext)) {
        return ExtensionVerdict::Binary;
    }
    if lookup(TEXT_EXTENSIONS, ext) {
        return ExtensionVerdict::Text;
    }
    ExtensionVerdict::Unknown
}

// The name's verdict for a path, without opening it.
pub(crate) fn verdict_for(path: &Path) -> ExtensionVerdict {
    path.file_name()
        .and_then(|name| name.to_str())
        .map_or(ExtensionVerdict::Unknown, extension_verdict)
}

// The walk's first answer: the name decides unless it is mixed-use.
pub(crate) fn initial_verdict(path: &Path) -> (bool, ExtensionVerdict) {
    match verdict_for(path) {
        ExtensionVerdict::Binary => (true, ExtensionVerdict::Binary),
        ExtensionVerdict::Text => (false, ExtensionVerdict::Text),
        verdict @ ExtensionVerdict::MixedUse => (starts_with_binary_content(path), verdict),
        ExtensionVerdict::Unknown => (false, ExtensionVerdict::Unknown),
    }
}

fn starts_with_binary_content(path: &Path) -> bool {
    use std::io::Read;
    let mut chunk = [0u8; 4096];
    std::fs::File::open(path)
        .and_then(|mut file| file.read(&mut chunk))
        .is_ok_and(|read| crate::types::detect_binary_content(&chunk[..read]))
}

fn extra_binary() -> &'static [String] {
    EXTRA_BINARY.get().map_or(&[], |extras| extras.as_ref())
}

// Adds operator extensions to the binary table and returns the merged size:
// additive only, so nothing removes a built-in entry. Values are trimmed,
// dot-stripped, lowercased and deduped; anything else is an error naming the
// value, text extensions included. First call wins.
pub fn add_extra_binary_extensions<S: AsRef<str>>(exts: &[S]) -> Result<usize, String> {
    let mut merged: Vec<String> = Vec::with_capacity(exts.len());
    for raw in exts {
        let value = raw.as_ref();
        let ext = value.trim().trim_start_matches('.').to_ascii_lowercase();
        if !is_bare_extension(&ext) {
            return Err(format!(
                "{value:?} is not a bare file extension: start with a letter or digit, then letters, \
                 digits, '-', '_', '+' or '~'"
            ));
        }
        // Text read as binary is a silently missed grep match, so that table stays closed.
        if lookup(TEXT_EXTENSIONS, &ext) {
            return Err(format!(
                "{value:?} is on the text table: classifying text as binary would silently lose \
                 content matches"
            ));
        }
        // A built-in entry already answers for its extension, and repeats are one item.
        if !lookup(BINARY_EXTENSIONS, &ext) && !merged.contains(&ext) {
            merged.push(ext);
        }
    }
    let _ = EXTRA_BINARY.set(merged.into_boxed_slice());
    Ok(BINARY_EXTENSIONS.len() + extra_binary().len())
}

fn is_bare_extension(ext: &str) -> bool {
    let mut chars = ext.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '+' | '~'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_tables_are_sorted_lowercase_and_disjoint() {
        for table in [BINARY_EXTENSIONS, TEXT_EXTENSIONS, MIXED_USE_EXTENSIONS] {
            assert!(table.is_sorted(), "{table:?}");
            assert!(
                !table
                    .iter()
                    .any(|ext| ext.bytes().any(|b| b.is_ascii_uppercase())),
                "{table:?}"
            );
        }
        for ext in TEXT_EXTENSIONS {
            assert!(
                BINARY_EXTENSIONS.binary_search(ext).is_err(),
                "{ext} is on both tables"
            );
        }
        for ext in MIXED_USE_EXTENSIONS {
            assert!(
                BINARY_EXTENSIONS.binary_search(ext).is_ok(),
                "{ext} is mixed-use but not binary, so the byte path is never reached"
            );
        }
    }

    // A binary extension must not be readable: the verdict comes from the name
    // whether or not the file exists, so this asserts the read never happened.
    #[test]
    fn a_binary_extension_is_judged_without_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("photo.png");

        assert_eq!(initial_verdict(&missing), (true, ExtensionVerdict::Binary));
        assert_eq!(verdict_for(&missing), ExtensionVerdict::Binary);
        assert!(!missing.exists());
    }

    // Same proof from the other side: the content says binary, the name says
    // text, and the name wins because nothing was read.
    #[test]
    fn a_text_extension_is_judged_without_reading_binary_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("payload.rs");
        std::fs::write(&path, [0u8; 64]).unwrap();

        assert_eq!(initial_verdict(&path), (false, ExtensionVerdict::Text));

        // The contrast: `.txt` is not on the table, so its bytes still decide.
        let mixed = dir.path().join("payload.txt");
        std::fs::write(&mixed, [0u8; 64]).unwrap();
        assert_eq!(verdict_for(&mixed), ExtensionVerdict::Unknown);
        assert!(crate::types::detect_binary_content(&[0u8; 64]));
    }

    #[test]
    fn mixed_use_and_unknown_names_keep_the_byte_path() {
        let dir = tempfile::tempdir().unwrap();
        let blueprint = dir.path().join("window.blp");
        std::fs::write(&blueprint, "template $Window : Gtk.Window {\n}\n").unwrap();
        let texture = dir.path().join("texture.blp");
        std::fs::write(&texture, b"BLP2\0\0\0").unwrap();

        assert_eq!(verdict_for(&blueprint), ExtensionVerdict::MixedUse);
        assert_eq!(
            initial_verdict(&blueprint),
            (false, ExtensionVerdict::MixedUse)
        );
        assert_eq!(
            initial_verdict(&texture),
            (true, ExtensionVerdict::MixedUse)
        );

        for name in [
            "no_extension",
            "photo.unknownext",
            "notes.txt",
            ".gitignore",
            "a.",
        ] {
            assert_eq!(
                verdict_for(Path::new(name)),
                ExtensionVerdict::Unknown,
                "{name}"
            );
        }
    }

    #[test]
    fn a_path_with_dots_in_its_directories_names_the_file() {
        assert_eq!(extension_verdict("a.b/plain"), ExtensionVerdict::Unknown);
        assert_eq!(extension_verdict("a.b/main.rs"), ExtensionVerdict::Text);
        assert_eq!(extension_verdict("a.b/photo.PNG"), ExtensionVerdict::Binary);
        assert_eq!(extension_verdict("Makefile"), ExtensionVerdict::Unknown);
    }

    #[test]
    fn counts_split_by_source() {
        let mut counts = VerdictCounts::default();
        for verdict in [
            ExtensionVerdict::Binary,
            ExtensionVerdict::Binary,
            ExtensionVerdict::Text,
            ExtensionVerdict::MixedUse,
            ExtensionVerdict::Unknown,
        ] {
            counts.record(verdict);
        }

        assert_eq!(counts.binary, 2);
        assert_eq!(counts.text, 1);
        assert_eq!(counts.mixed_use, 1);
        assert_eq!(counts.unknown, 1);
    }

    // The slice-04 seam: extras are additive, normalized and deduped.
    #[test]
    fn configured_extensions_join_the_table_additively() {
        let extra = vec![
            "  .ATLAS ".to_string(),
            "atlas".to_string(),
            "png".to_string(),
        ];
        let merged = add_extra_binary_extensions(&extra).unwrap();

        assert_eq!(merged, BINARY_EXTENSIONS.len() + 1, "one new extension");
        assert_eq!(extension_verdict("wall.atlas"), ExtensionVerdict::Binary);
        assert_eq!(extension_verdict("wall.png"), ExtensionVerdict::Binary);
        // Nothing can be taken off the table.
        assert!(add_extra_binary_extensions::<&str>(&[]).is_ok());
        assert_eq!(extension_verdict("wall.atlas"), ExtensionVerdict::Binary);
    }

    #[test]
    fn a_bad_configured_extension_names_itself() {
        for bad in ["", "  ", "tar.gz", "a/b", "c:\\x", ".", "a b"] {
            let message = add_extra_binary_extensions(&[bad]).unwrap_err();
            assert!(message.contains(&format!("{bad:?}")), "{message}");
        }
    }

    // No spelling removes an entry: a removal prefix is not an extension, and
    // the text table refuses rather than reclassifies.
    #[test]
    fn removals_and_text_extensions_are_refused() {
        for attempt in ["!png", "-png", "*", "!all"] {
            let message = add_extra_binary_extensions(&[attempt]).unwrap_err();
            assert!(message.contains("not a bare file extension"), "{message}");
        }
        for text in ["rs", ".RS", "md", "svg"] {
            let message = add_extra_binary_extensions(&[text]).unwrap_err();
            assert!(message.contains("text table"), "{message}");
        }
    }

    // An empty list is the built-in table, and no extra list demotes an entry.
    #[test]
    fn extras_only_add_binary_and_never_remove() {
        let extras = vec!["mtl".to_string(), "atlas".to_string()];
        for list in [Vec::new(), extras.clone()] {
            for ext in BINARY_EXTENSIONS {
                assert!(
                    extension_verdict_with(&format!("x.{ext}"), &list).names_binary(),
                    "{ext} with {list:?}"
                );
            }
            for ext in TEXT_EXTENSIONS {
                assert_eq!(
                    extension_verdict_with(&format!("x.{ext}"), &list),
                    ExtensionVerdict::Text,
                    "{ext} with {list:?}"
                );
            }
            for ext in MIXED_USE_EXTENSIONS {
                assert_eq!(
                    extension_verdict_with(&format!("x.{ext}"), &list),
                    ExtensionVerdict::MixedUse,
                    "{ext} left the byte path"
                );
            }
        }
        // Only the extra list answers for these, and the verdict is the name's:
        // neither path here was ever opened.
        assert_eq!(
            extension_verdict_with("x.mtl", &extras),
            ExtensionVerdict::Binary
        );
        assert_eq!(
            extension_verdict_with("x.mtl", &[]),
            ExtensionVerdict::Unknown
        );
    }
}
