//! The only module that talks to `eso_common`. Everything else sees `Vec<u8>` / `&[u8]`
//! and the plain types defined here.

use std::collections::HashMap;
use std::path::Path;

use eso_common::mnf::{MnfFile, MnfFileEntry};
use eso_common::util::guess_file_extension;

/// Entry id of an `en.lang`-style table: `group id -> (record index -> text)`.
/// Texts are in the form the old `en.lang.csv` round trip produced (`\r` / `\n` escaped as the two
/// characters `\r` / `\n`, cut at the first NUL), so existing consumers see identical strings.
#[derive(Default)]
pub struct LangTable {
    by_group: HashMap<i32, HashMap<u32, String>>,
    empty: HashMap<u32, String>,
}

impl LangTable {
    /// `group` is the decimal id the old CSV printed (signed 32-bit), e.g. `"162658389"`.
    pub fn group(&self, group: &str) -> &HashMap<u32, String> {
        group.trim().parse::<i32>().ok().and_then(|g| self.by_group.get(&g)).unwrap_or(&self.empty)
    }
}

/// Header line of the old `eso_mnf.csv`.
pub const MANIFEST_HEADER: &str = " Index ,    ID1    , FileIndex ,   Unk1    ,    Size   ,   ZSize   ,    Hash   ,   Offset  , ZTyp, Arch,  Unk2 , Filename";

pub struct Archive {
    mnf: MnfFile,
    lang: std::cell::OnceCell<Result<LangTable, String>>,
}

impl Archive {
    /// Once per process: load Oodle (explicit path first), the MNF and its ZOSFT.
    pub fn open(mnf_path: &Path, oodle: Option<&Path>) -> Result<Archive, String> {
        // Library chatter goes nowhere; errors are reported through the returned values.
        eso_common::log::set_console_enabled(false);

        let oodle_str = oodle.map(|p| p.to_string_lossy().into_owned());
        if let Err(e) = eso_common::oodle::try_load_oodle_lib_from(oodle_str.as_deref()) {
            eprintln!("Warning: {} (looked in {:?}); entries that are Oodle-compressed will fail to read.", e, oodle_str);
        }

        let mut mnf = MnfFile::new();
        mnf.load(&mnf_path.to_string_lossy())
            .map_err(|e| format!("Failed to load '{}': {}", mnf_path.display(), e))?;
        if let Err(e) = mnf.load_zosft() {
            // Not fatal: only file names are lost.
            eprintln!("Warning: ZOSFT not loaded ({}); entry names are unavailable.", e);
        }
        for (i, e) in mnf.file_table().iter().enumerate() {
            if e.index as usize != i {
                return Err(format!("MNF file table is not position-indexed (entry at {} has index {})", i, e.index));
            }
        }
        Ok(Archive { mnf, lang: std::cell::OnceCell::new() })
    }

    /// Position in the slice == `entry.index` (the absolute index).
    pub fn entries(&self) -> &[MnfFileEntry] {
        self.mnf.file_table()
    }

    pub fn name(&self, e: &MnfFileEntry) -> Option<&str> {
        self.mnf.zosft_name(e).filter(|n| !n.is_empty())
    }

    fn entry(&self, index: u32) -> Result<&MnfFileEntry, String> {
        self.entries().get(index as usize).ok_or_else(|| format!("entry {} is not in the MNF file table", index))
    }

    /// Decompressed bytes of one entry.
    pub fn read(&self, index: u32) -> Result<Vec<u8>, String> {
        let e = *self.entry(index)?;
        self.mnf.read_entry(&e, None).map_err(|e| format!("entry {}: {}", index, e))
    }

    /// Streaming bulk read: archive/offset order, one DAT open per archive, each result handed to `f`
    /// and dropped afterwards. A failing entry is reported to `f`; it never aborts the batch.
    pub fn read_each(&self, wanted: &[u32], mut f: impl FnMut(&MnfFileEntry, Result<Vec<u8>, String>)) {
        let table = self.entries();
        self.mnf.read_entries_each(wanted, |idx, r| {
            let r = r.map_err(|e| format!("entry {}: {}", idx, e));
            match table.get(idx as usize) {
                Some(e) => f(e, r),
                None => eprintln!("Warning: {}", r.err().unwrap_or_else(|| format!("entry {} unknown", idx))),
            }
        });
    }

    /// The EsoFileData table at `index`, in the byte layout of the old `*_Uncompressed.EsoFileData` files.
    pub fn table(&self, index: u32) -> Result<Vec<u8>, String> {
        let e = *self.entry(index)?;
        let t = self.mnf.read_eso_file_data(&e).map_err(|e| format!("table {}: {}", index, e))?;
        Ok(t.to_combined_bytes())
    }

    /// One line of the old `eso_mnf.csv` (same format as `MnfFile::dump_file_table`), without the newline.
    pub fn manifest_line(&self, e: &MnfFileEntry) -> String {
        format!("{:7}, 0x{:08X}, 0x{:08X}, 0x{:08X}, 0x{:08X}, 0x{:08X}, 0x{:08X}, 0x{:08X}, 0x{:02X}, 0x{:02X}, 0x{:04X}, {}",
            e.index as i32, e.id1, e.file_index, e.unknown1, e.size, e.compressed_size, e.hash, e.offset,
            e.compress_type, e.archive_index, e.unknown2, self.name(e).unwrap_or(""))
    }

    /// `en.lang`, parsed once and cached.
    pub fn lang(&self) -> Result<&LangTable, String> {
        self.lang.get_or_init(|| self.load_lang()).as_ref().map_err(|e| e.clone())
    }

    fn load_lang(&self) -> Result<LangTable, String> {
        let is_en = |n: &str| n.rsplit(|c| c == '/' || c == '\\').next() == Some("en.lang");
        let hits: Vec<&MnfFileEntry> = self.entries().iter()
            .filter(|e| self.name(e).map(is_en).unwrap_or(false))
            .collect();
        let Some(first) = hits.first() else {
            return Err("en.lang was not found in the archive (no ZOSFT name matches)".to_string());
        };
        if hits.len() > 1 {
            eprintln!("Warning: {} entries are named en.lang ({}); using the first (index {}).",
                hits.len(), hits.iter().map(|e| e.index.to_string()).collect::<Vec<_>>().join(", "), first.index);
        }
        let bytes = self.read(first.index)?;
        let mut lang = eso_common::lang::EsoLangFile::new();
        lang.parse_data(&bytes).map_err(|e| format!("en.lang: {}", e))?;

        let mut t = LangTable::default();
        for r in lang.records() {
            // The CSV printed the index as i32 and the reader parsed it as u32: negative ones were dropped.
            if r.index > i32::MAX as u32 { continue; }
            let text = r.text.split('\0').next().unwrap_or("").replace('\r', "\\r").replace('\n', "\\n");
            t.by_group.entry(r.id as i32).or_default().insert(r.index, text);
        }
        Ok(t)
    }
}

pub fn dds_from_bytes(bytes: Vec<u8>) -> Result<Vec<u8>, String> {
    match guess_file_extension(&bytes) {
        "dds" => Ok(bytes),
        "xv4" if bytes.len() > 12 => Ok(bytes[12..].to_vec()),
        other => Err(format!("not a texture (looks like '{}')", other)),
    }
}
