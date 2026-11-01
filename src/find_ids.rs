//! `--find-ids`: work out which EsoFileData table maps terrain-layer ids (e.g. the kind-6 water layers)
//! to their properties (swimmable / slaughterfish, lava, texture, ...).
//!
//! Why not just grep for the numbers? Every table is full of small integers (counts, flags, row ids up to
//! ~150k), so a single hit of "4841" in a table means almost nothing. Instead every table is scanned for
//! the WHOLE set of ids you give it and each *column* is scored on its own:
//!
//!   * A column is (table, byte offset, endianness) - rows of one table share a schema, so a real
//!     reference/key column hits at the SAME offset in many rows.
//!   * Also considered as columns: the record's row key (the u32 in the `####` sub-header, which is the
//!     row id the game uses) and the row position.
//!   * Score = weighted fraction of your distinct ids that show up in that one column. Small ids (< 256,
//!     < 1024) are down-weighted because they collide with ordinary counters/flags by chance.
//!   * Ties are broken by "uniqueness" (distinct ids / hits: ~1.0 means the column is a primary key, which
//!     is what a lookup table looks like) and then by smaller tables.
//!
//! A table whose key/column covers most of your ids is the lookup table you want; the dump then shows
//! the whole row (all u32 words + embedded strings) for every matched id so you can see which words
//! change with water type.
//!
//! Layout of a table (what `Archive::table` returns), all big endian:
//!   16 byte header  FA FA EB EB | u32 | u32 entry_count | u32
//!   per record      "####" | pos | usize1 | usize2 | csize | KEY | orig_off | LEN | payload[LEN]

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

const MAGIC: [u8; 4] = [0xFA, 0xFA, 0xEB, 0xEB];
const HDR: usize = 16;
const REC_HDR: usize = 32;

// ------------------------------------------------------------------------------------------------
// Options
// ------------------------------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct Opts {
    /// The ids to look for (layer ids from a legend). 0 is ignored: it is "no data".
    pub ids: Vec<u32>,
    /// Restrict to these table FileIndex values (hex/dec). Empty = every candidate table.
    pub tables: Vec<u32>,
    /// Probe every manifest entry instead of just those whose Unk1 category is 0x6xxxxxxx.
    pub any: bool,
    /// How many payload bytes at the start of each row are scanned.
    pub window: usize,
    /// How many ranked columns to print.
    pub top: usize,
    /// How many of the best columns get a full CSV dump of the matched rows.
    pub dump: usize,
    /// Compare mode: ids known to HAVE the property (e.g. water) ...
    pub yes: Vec<u32>,
    /// ... and ids known NOT to have it. Empty = every other id in `ids`.
    pub no: Vec<u32>,
    /// Skip tables with more rows than this (0 = no limit). Terrain-style tables are ~1000-3000 rows; huge tables (weapons, effects...)
    /// share the same id numbers and only produce chance matches.
    pub max_rows: usize,
    /// Rows mode: print these ids' rows side by side, showing only the bytes that differ.
    pub rows: Vec<u32>,
    /// Same-value mode: ids that should share a rare byte value (e.g. every water layer).
    pub same: Vec<u32>,
}

impl Default for Opts {
    fn default() -> Self {
        Opts { ids: Vec::new(), tables: Vec::new(), any: false, window: 256, top: 15, dump: 3, yes: Vec::new(), no: Vec::new(), max_rows: 0, rows: Vec::new(), same: Vec::new() }
    }
}

/// Read the `layer_id` column of a legend written by `--terrain-layers` (`k<kind>_legend.csv`).
pub fn ids_from_legend(path: &Path) -> Result<Vec<u32>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read '{}': {}", path.display(), e))?;
    let mut out = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let first = line.split(',').next().unwrap_or("").trim();
        match first.parse::<u32>() {
            Ok(v) => out.push(v),
            Err(_) if n == 0 => {} // header
            Err(_) => return Err(format!("'{}' line {}: bad layer id '{}'", path.display(), n + 1, first)),
        }
    }
    Ok(out)
}

// ------------------------------------------------------------------------------------------------
// Table parsing
// ------------------------------------------------------------------------------------------------

fn be32(b: &[u8], o: usize) -> u32 {
    u32::from_be_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

pub struct Row<'a> {
    /// 1-based position written by the exporter.
    pub pos: u32,
    /// The row key (sub-header "index"): the id the game looks the row up by.
    pub key: u32,
    pub payload: &'a [u8],
}

/// Split a combined table into rows. Rows are delimited by `####` and sized by their length field; if the
/// bytes at the expected position are not `####` (truncated/odd record) it resyncs on the next `####`.
pub fn parse_rows(d: &[u8]) -> Vec<Row<'_>> {
    let mut out = Vec::new();
    if d.len() < HDR || d[0..4] != MAGIC {
        return out;
    }
    let count = be32(d, 8) as usize;
    let mut pos = HDR;
    while out.len() < count && pos + REC_HDR <= d.len() {
        if &d[pos..pos + 4] != b"####" {
            match d[pos..].windows(4).position(|w| w == b"####") {
                Some(skip) => { pos += skip; continue; }
                None => break,
            }
        }
        let len = be32(d, pos + 28) as usize;
        let start = pos + REC_HDR;
        let end = start.saturating_add(len).min(d.len());
        out.push(Row { pos: be32(d, pos + 4), key: be32(d, pos + 20), payload: &d[start..end] });
        pos = end;
    }
    out
}

// ------------------------------------------------------------------------------------------------
// Column scanning
// ------------------------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub enum Kind {
    /// Row position (1-based).
    Pos,
    /// Row key from the record sub-header.
    Key,
    /// u32 big endian at a payload offset.
    Be,
    /// u32 little endian at a payload offset.
    Le,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub struct Col {
    pub kind: Kind,
    pub off: usize,
}

impl Col {
    pub fn label(&self) -> String {
        match self.kind {
            Kind::Pos => "row position".to_string(),
            Kind::Key => "row key".to_string(),
            Kind::Be => format!("u32 BE @ +{}", self.off),
            Kind::Le => format!("u32 LE @ +{}", self.off),
        }
    }
    pub fn short(&self) -> String {
        match self.kind {
            Kind::Pos => "pos".to_string(),
            Kind::Key => "key".to_string(),
            Kind::Be => format!("be{}", self.off),
            Kind::Le => format!("le{}", self.off),
        }
    }
    /// The value of this column in a row (None if the row is too short).
    pub fn value(&self, r: &Row) -> Option<u32> {
        let rd = |o: usize| r.payload.get(o..o + 4).map(|b| [b[0], b[1], b[2], b[3]]);
        match self.kind {
            Kind::Pos => Some(r.pos),
            Kind::Key => Some(r.key),
            Kind::Be => rd(self.off).map(u32::from_be_bytes),
            Kind::Le => rd(self.off).map(u32::from_le_bytes),
        }
    }
}

/// Set of wanted ids with a flat bitmap for the hot loop.
pub struct Targets {
    bits: Vec<bool>,
    max: u32,
    pub ids: BTreeSet<u32>,
}

impl Targets {
    pub fn new(ids: &[u32]) -> Result<Targets, String> {
        let ids: BTreeSet<u32> = ids.iter().copied().filter(|&i| i != 0).collect();
        let max = *ids.iter().next_back().ok_or("no ids to search for (0 is ignored)")?;
        if max > 1 << 26 {
            return Err(format!("id {} is too large for the bitmap (limit {}).", max, 1u32 << 26));
        }
        let mut bits = vec![false; max as usize + 1];
        for &i in &ids { bits[i as usize] = true; }
        Ok(Targets { bits, max, ids })
    }
    #[inline]
    pub fn has(&self, v: u32) -> bool {
        v <= self.max && self.bits[v as usize]
    }
    #[inline]
    pub fn plausible(&self, v: u32) -> bool {
        v <= self.max
    }
    /// Small ids collide with ordinary counters/flags by chance, so they count for less.
    pub fn weight(id: u32) -> f64 {
        if id < 256 { 0.1 } else if id < 1024 { 0.5 } else { 1.0 }
    }
    pub fn total_weight(&self) -> f64 {
        self.ids.iter().map(|&i| Self::weight(i)).sum()
    }
}

#[derive(Default, Debug)]
pub struct ColStat {
    pub ids: BTreeSet<u32>,
    pub hits: u32,
}

pub struct TableScan {
    pub rows: usize,
    pub cols: BTreeMap<Col, ColStat>,
    /// Rows whose value at this offset is <= the largest wanted id (how "id-like" the column is).
    pub plaus_be: Vec<u32>,
    pub plaus_le: Vec<u32>,
    pub plaus_key: u32,
}

pub fn scan_rows(rows: &[Row], t: &Targets, window: usize) -> TableScan {
    let mut cols: BTreeMap<Col, ColStat> = BTreeMap::new();
    let mut plaus_be = vec![0u32; window];
    let mut plaus_le = vec![0u32; window];
    let mut plaus_key = 0u32;
    let mut note = |cols: &mut BTreeMap<Col, ColStat>, c: Col, v: u32| {
        let s = cols.entry(c).or_default();
        s.ids.insert(v);
        s.hits += 1;
    };
    for r in rows {
        // Row position is 1..N by construction, so it is never evidence and is not scored.
        if t.plausible(r.key) { plaus_key += 1; }
        if t.has(r.key) { note(&mut cols, Col { kind: Kind::Key, off: 0 }, r.key); }
        let p = r.payload;
        let n = p.len().min(window);
        if n < 4 { continue; }
        for o in 0..=(n - 4) {
            let b = [p[o], p[o + 1], p[o + 2], p[o + 3]];
            let be = u32::from_be_bytes(b);
            let le = u32::from_le_bytes(b);
            if t.plausible(be) {
                plaus_be[o] += 1;
                if t.has(be) { note(&mut cols, Col { kind: Kind::Be, off: o }, be); }
            }
            if t.plausible(le) {
                plaus_le[o] += 1;
                if t.has(le) { note(&mut cols, Col { kind: Kind::Le, off: o }, le); }
            }
        }
    }
    TableScan { rows: rows.len(), cols, plaus_be, plaus_le, plaus_key }
}

// ------------------------------------------------------------------------------------------------
// Ranking
// ------------------------------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct Cand {
    pub entry: u32,
    pub file_index: u32,
    pub unk1: u32,
    pub arch: u8,
    pub name: String,
    pub rows: usize,
    pub col: Col,
    pub distinct: usize,
    pub hits: u32,
    /// Weighted fraction of the wanted ids present in this column (0..1).
    pub cover: f64,
    /// distinct / hits: ~1 = key-like column.
    pub uniq: f64,
    /// Distinct ids expected by pure chance if the column's id-like values were spread evenly over 0..=max id.
    /// A dense column (row keys 1..N) expects nearly all of them, so it proves nothing.
    pub expected: f64,
    /// distinct - expected: the evidence. Ranking is by this.
    pub excess: f64,
    /// % of rows whose value at this column is <= max wanted id.
    pub idlike: f64,
}

pub fn candidates(entry: u32, file_index: u32, unk1: u32, arch: u8, name: &str, s: &TableScan, t: &Targets, min_distinct: usize) -> Vec<Cand> {
    let total = t.total_weight();
    let mut out = Vec::new();
    for (col, st) in &s.cols {
        if st.ids.len() < min_distinct || col.kind == Kind::Pos { continue; }
        let w: f64 = st.ids.iter().map(|&i| Targets::weight(i)).sum();
        let plaus = match col.kind {
            Kind::Be => s.plaus_be.get(col.off).copied().unwrap_or(0) as f64,
            Kind::Le => s.plaus_le.get(col.off).copied().unwrap_or(0) as f64,
            _ => s.plaus_key as f64,
        };
        let n = t.ids.len() as f64;
        let expected = n * (plaus / (t.max as f64 + 1.0)).min(1.0);
        out.push(Cand {
            entry, file_index, unk1, arch, name: name.to_string(), rows: s.rows, col: *col,
            distinct: st.ids.len(), hits: st.hits,
            expected, excess: st.ids.len() as f64 - expected,
            cover: if total > 0.0 { w / total } else { 0.0 },
            uniq: st.ids.len() as f64 / st.hits.max(1) as f64,
            idlike: if s.rows > 0 { 100.0 * plaus / s.rows as f64 } else { 0.0 },
        });
    }
    out
}

pub fn rank(c: &mut Vec<Cand>) {
    c.sort_by(|a, b| {
        b.excess.partial_cmp(&a.excess).unwrap_or(std::cmp::Ordering::Equal)
            .then(b.uniq.partial_cmp(&a.uniq).unwrap_or(std::cmp::Ordering::Equal))
            .then(a.rows.cmp(&b.rows))
            // BE before LE, key before offsets, lower offsets first: fully deterministic
            .then(a.col.cmp(&b.col))
    });
}

// ------------------------------------------------------------------------------------------------
// Row display
// ------------------------------------------------------------------------------------------------

/// Printable ASCII runs of at least `min` bytes (texture names, flags written as text, ...).
pub fn strings(p: &[u8], min: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for &b in p {
        if (0x20..0x7F).contains(&b) {
            cur.push(b as char);
        } else {
            if cur.len() >= min { out.push(std::mem::take(&mut cur)); } else { cur.clear(); }
        }
    }
    if cur.len() >= min { out.push(cur); }
    out
}

fn words(p: &[u8], n: usize) -> Vec<u32> {
    p.chunks_exact(4).take(n).map(|c| u32::from_be_bytes([c[0], c[1], c[2], c[3]])).collect()
}

fn csv_q(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

/// Write every row of the table whose `col` value is a wanted id: id, row pos/key/len, the first 32 words
/// (BE hex, word k = payload byte offset 4k) and any text. Returns rows written.
pub fn dump_matches(rows: &[Row], col: Col, t: &Targets, path: &Path) -> Result<usize, String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("failed to create '{}': {}", dir.display(), e))?;
    }
    let io = |e: std::io::Error| format!("failed to write '{}': {}", path.display(), e);
    let mut f = BufWriter::new(File::create(path).map_err(io)?);
    let mut head = String::from("id,row_pos,row_key,payload_len,strings");
    for k in 0..32 { head.push_str(&format!(",w{}", k * 4)); }
    writeln!(f, "{}", head).map_err(io)?;
    let mut n = 0;
    let mut matched: Vec<(u32, &Row)> = rows.iter().filter_map(|r| col.value(r).filter(|v| t.has(*v)).map(|v| (v, r))).collect();
    matched.sort_by_key(|(v, r)| (*v, r.pos));
    for (id, r) in matched {
        let w = words(r.payload, 32).iter().map(|x| format!("{:08X}", x)).collect::<Vec<_>>().join(",");
        writeln!(f, "{},{},{},{},{},{}", id, r.pos, r.key, r.payload.len(), csv_q(&strings(r.payload, 4).join(" | ")), w).map_err(io)?;
        n += 1;
    }
    f.flush().map_err(io)?;
    Ok(n)
}

/// Console preview of a few matched rows.
pub fn preview(rows: &[Row], col: Col, t: &Targets, limit: usize) {
    let mut shown = 0;
    for r in rows {
        let Some(v) = col.value(r) else { continue };
        if !t.has(v) { continue; }
        let w = words(r.payload, 14).iter().map(|x| format!("{:08X}", x)).collect::<Vec<_>>().join(" ");
        let s = strings(r.payload, 4);
        println!("      id {:>6}  row {:>6} key {:>8}  len {:>4}  {}{}", v, r.pos, r.key, r.payload.len(), w,
            if s.is_empty() { String::new() } else { format!("  text: {}", s.join(" | ")) });
        shown += 1;
        if shown >= limit { break; }
    }
}

// ------------------------------------------------------------------------------------------------
// Driver
// ------------------------------------------------------------------------------------------------

/// Everything `run` needs from the archive, so the search itself can be tested without game files.
pub struct TableRef {
    pub entry: u32,
    pub file_index: u32,
    pub unk1: u32,
    pub arch: u8,
    pub offset: u32,
    pub name: String,
}

// ------------------------------------------------------------------------------------------------
// Compare mode: which payload byte separates "yes" ids from "no" ids?
// ------------------------------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct Sep {
    pub off: usize,
    /// The byte value that best separates the groups.
    pub value: u8,
    pub frac_yes: f64,
    pub frac_no: f64,
    pub n_yes: usize,
    pub n_no: usize,
    /// Most common byte in each group and the share of the group that has it.
    pub mode_yes: (u8, f64),
    pub mode_no: (u8, f64),
}

impl Sep {
    /// A real flag: every yes-row has one value, every no-row has another.
    pub fn clean(&self) -> bool {
        self.mode_yes.0 != self.mode_no.0 && self.mode_yes.1 >= 0.999 && self.mode_no.1 >= 0.999
    }
    /// |share of yes-rows with the value - share of no-rows with it|: 1.0 = perfect separator.
    pub fn score(&self) -> f64 { (self.frac_yes - self.frac_no).abs() }
}

/// Best separating byte per payload offset (offsets 0..4 are the key, skipped). Rows are matched by row key.
/// Needs rows from both groups; `None` if either group is under-represented in this table.
pub fn separate(rows: &[Row], yes: &BTreeSet<u32>, no: &BTreeSet<u32>, window: usize) -> Option<(Vec<Sep>, usize, usize)> {
    let ys: Vec<&Row> = rows.iter().filter(|r| yes.contains(&r.key)).collect();
    let ns: Vec<&Row> = rows.iter().filter(|r| no.contains(&r.key)).collect();
    let need = |g: &BTreeSet<u32>| 2.max(g.len().div_ceil(2));
    if ys.len() < need(yes) || ns.len() < need(no) { return None; }
    let mut out = Vec::new();
    for o in 4..window {
        let (mut cy, mut cn) = ([0u32; 256], [0u32; 256]);
        let (mut ny, mut nn) = (0usize, 0usize);
        for r in &ys { if let Some(&b) = r.payload.get(o) { cy[b as usize] += 1; ny += 1; } }
        for r in &ns { if let Some(&b) = r.payload.get(o) { cn[b as usize] += 1; nn += 1; } }
        // both groups must mostly have this offset (rows can differ in length)
        if ny * 4 < ys.len() * 3 || nn * 4 < ns.len() * 3 { continue; }
        let best = (0..256usize)
            .map(|v| (v, (cy[v] as f64 / ny as f64 - cn[v] as f64 / nn as f64)))
            .max_by(|a, b| a.1.abs().partial_cmp(&b.1.abs()).unwrap_or(std::cmp::Ordering::Equal)
                .then(a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal)) // tie: the value the YES group has
                .then(b.0.cmp(&a.0)))
            .unwrap();
        let mode = |c: &[u32; 256], n: usize| { let (v, k) = c.iter().enumerate().max_by_key(|(i, k)| (**k, std::cmp::Reverse(*i))).unwrap(); (v as u8, *k as f64 / n as f64) };
        out.push(Sep { mode_yes: mode(&cy, ny), mode_no: mode(&cn, nn), off: o, value: best.0 as u8, frac_yes: cy[best.0] as f64 / ny as f64, frac_no: cn[best.0] as f64 / nn as f64, n_yes: ny, n_no: nn });
    }
    Some((out, ys.len(), ns.len()))
}

fn run_compare(archive: &crate::archive::Archive, opts: &Opts, out_dir: &Path) -> Result<(), String> {
    let yes: BTreeSet<u32> = opts.yes.iter().copied().filter(|&i| i != 0).collect();
    let mut no: BTreeSet<u32> = opts.no.iter().copied().filter(|&i| i != 0).collect();
    if no.is_empty() {
        no = opts.ids.iter().copied().filter(|&i| i != 0 && !yes.contains(&i)).collect();
    }
    if yes.is_empty() || no.is_empty() {
        return Err("compare mode needs ids with the property (--find-yes) and ids without it (--find-no, or --find-legend/--find-ids for 'everything else').".to_string());
    }
    println!("Compare mode: {} 'yes' id(s) vs {} 'no' id(s). A perfect separator byte has yes-share 100% and no-share 0% (or the reverse).", yes.len(), no.len());
    if yes.len().min(no.len()) < 6 {
        println!("Warning: a group has under 6 ids; with so few, bytes separate them by pure chance. Treat results as hints; add more ids.");
    }
    let mut refs: Vec<(u32, u32, u8, u32, String)> = archive.entries().iter()
        .filter(|e| opts.any || (e.unknown1 >> 28) == 0x6)
        .filter(|e| opts.tables.is_empty() || opts.tables.contains(&e.file_index))
        .filter(|e| e.size >= (HDR + REC_HDR) as u32)
        .map(|e| (e.index, e.file_index, e.archive_index, e.offset, archive.name(e).unwrap_or("").to_string()))
        .collect();
    refs.sort_by_key(|r| (r.2, r.3));
    println!("Checking {} table entr{}...", refs.len(), if refs.len() == 1 { "y" } else { "ies" });

    struct Res { entry: u32, fi: u32, rows: usize, ny: usize, nn: usize, sep: Sep, name: String }
    let mut all: Vec<Res> = Vec::new();
    for (n, r) in refs.iter().enumerate() {
        let Ok(bytes) = archive.table(r.0) else { continue };
        let rows = parse_rows(&bytes);
        if opts.max_rows > 0 && rows.len() > opts.max_rows { continue; }
        if let Some((seps, ny, nn)) = separate(&rows, &yes, &no, opts.window) {
            for sp in seps {
                if sp.score() >= 0.8 { all.push(Res { entry: r.0, fi: r.1, rows: rows.len(), ny, nn, sep: sp, name: r.4.clone() }); }
            }
        }
        if (n + 1) % 200 == 0 { eprintln!("  ...{}/{} entries", n + 1, refs.len()); }
    }
    all.sort_by(|a, b| b.sep.clean().cmp(&a.sep.clean()).then(b.sep.score().partial_cmp(&a.sep.score()).unwrap_or(std::cmp::Ordering::Equal))
        .then((b.ny + b.nn).cmp(&(a.ny + a.nn))).then(a.rows.cmp(&b.rows)).then(a.entry.cmp(&b.entry)).then(a.sep.off.cmp(&b.sep.off)));
    if all.is_empty() {
        println!("No payload byte separates the groups by 80% or more in any table.");
        println!("The flag may be a multi-byte value, a bit inside a byte, in another table, or --find-window may be too small.");
        return Ok(());
    }
    println!("\n{:>3}  {:>7} {:>10} {:>6} {:>9}  {:>6} {:>6}  {:>10} {:>10}  sep  clean", "#", "entry", "FileIndex", "rows", "yes/no", "offset", "byte", "yes-share", "no-share");
    let clean = all.iter().filter(|r| r.sep.clean()).count();
    println!("{} separating byte(s), {} of them clean (both groups uniform).", all.len(), clean);
    for (i, r) in all.iter().take(opts.top.max(25)).enumerate() {
        println!("{:>3}  {:>7} 0x{:08X} {:>6} {:>4}/{:<4}  {:>6} 0x{:02X}   {:>9.0}% {:>9.0}%  {:.2}  {}",
            i + 1, r.entry, r.fi, r.rows, r.ny, r.nn, format!("+{}", r.sep.off), r.sep.value, r.sep.frac_yes * 100.0, r.sep.frac_no * 100.0, r.sep.score(),
            if r.sep.clean() { format!("YES (yes=0x{:02X} no=0x{:02X})", r.sep.mode_yes.0, r.sep.mode_no.0) } else { String::new() });
    }
    let dir = out_dir.join("find_ids");
    std::fs::create_dir_all(&dir).map_err(|e| format!("failed to create '{}': {}", dir.display(), e))?;
    let p = dir.join("compare.csv");
    let io = |e: std::io::Error| format!("failed to write '{}': {}", p.display(), e);
    let mut f = BufWriter::new(File::create(&p).map_err(io)?);
    writeln!(f, "rank,entry,file_index,rows,yes_rows,no_rows,offset,byte,yes_share,no_share,separation,clean,yes_value,no_value,name").map_err(io)?;
    for (i, r) in all.iter().enumerate() {
        writeln!(f, "{},{},0x{:x},{},{},{},{},0x{:02X},{:.3},{:.3},{:.3},{},0x{:02X},0x{:02X},{}", i + 1, r.entry, r.fi, r.rows, r.ny, r.nn, r.sep.off, r.sep.value,
            r.sep.frac_yes, r.sep.frac_no, r.sep.score(), r.sep.clean(), r.sep.mode_yes.0, r.sep.mode_no.0, csv_q(&r.name)).map_err(io)?;
    }
    f.flush().map_err(io)?;
    println!("\nAll {} separating byte(s) -> {}", all.len(), p.display());
    Ok(())
}

/// Offsets where the given rows do not all hold the same byte, with each row's byte (None = row too short).
pub fn differing_offsets(rows: &[&Row], window: usize) -> Vec<(usize, Vec<Option<u8>>)> {
    let max = rows.iter().map(|r| r.payload.len()).max().unwrap_or(0).min(window);
    let mut out = Vec::new();
    for o in 0..max {
        let v: Vec<Option<u8>> = rows.iter().map(|r| r.payload.get(o).copied()).collect();
        if v.iter().any(|x| *x != v[0]) { out.push((o, v)); }
    }
    out
}

fn run_rows(archive: &crate::archive::Archive, opts: &Opts) -> Result<(), String> {
    let ids: BTreeSet<u32> = opts.rows.iter().copied().collect();
    let max_rows = if opts.max_rows == 0 { 5000 } else { opts.max_rows };
    let mut shown = 0;
    for e in archive.entries().iter()
        .filter(|e| opts.any || (e.unknown1 >> 28) == 0x6)
        .filter(|e| opts.tables.is_empty() || opts.tables.contains(&e.file_index))
        .filter(|e| e.size >= (HDR + REC_HDR) as u32)
    {
        let Ok(bytes) = archive.table(e.index) else { continue };
        let rows = parse_rows(&bytes);
        if rows.len() > max_rows { continue; }
        let mut sel: Vec<&Row> = ids.iter().filter_map(|id| rows.iter().find(|r| r.key == *id)).collect();
        // a table must hold most of the ids to be worth showing, unless the user named it
        if sel.len() < 2 || (opts.tables.is_empty() && sel.len() * 4 < ids.len() * 3) { continue; }
        sel.sort_by_key(|r| opts.rows.iter().position(|i| *i == r.key));
        println!("\n=== entry {} FileIndex 0x{:x}: {} rows, {}/{} ids present ===", e.index, e.file_index, rows.len(), sel.len(), ids.len());
        print!("  offset ");
        for r in &sel { print!("{:>7}", r.key); }
        println!("   (bytes in hex; only offsets that differ)");
        for (o, v) in differing_offsets(&sel, opts.window) {
            print!("  +{:<5} ", o);
            for b in v { match b { Some(b) => print!("{:>7}", format!("{:02X}", b)), None => print!("{:>7}", "--") } }
            println!();
        }
        shown += 1;
        if shown >= opts.top { println!("\n(stopped after {} tables; raise --find-top or narrow with --find-table)", shown); break; }
    }
    if shown == 0 { println!("No table (<= {} rows) holds enough of those ids; raise --find-max-rows or name tables with --find-table.", max_rows); }
    Ok(())
}

#[derive(Clone, Debug)]
pub struct Same {
    pub off: usize,
    pub value: u8,
    /// Share of ALL rows of the table that have this byte value here.
    pub table_share: f64,
    /// log10 of the chance that every group row would land on this value if rows were random: table_share^n. More negative = better.
    pub log10_p: f64,
}

/// Offsets where every row of the group holds the same byte AND that byte is rare in the table as a whole.
/// Unlike `separate` this needs no "no" group and copes with huge tables, because rarity is part of the score.
pub fn shared(rows: &[Row], group: &BTreeSet<u32>, window: usize) -> Option<(Vec<Same>, usize)> {
    let sel: Vec<&Row> = rows.iter().filter(|r| group.contains(&r.key)).collect();
    if sel.len() < 3.max((group.len() * 3).div_ceil(4)) { return None; }
    let mut out = Vec::new();
    for o in 4..window {
        let mut cg = [0u32; 256];
        let mut ng = 0usize;
        for r in &sel { if let Some(&b) = r.payload.get(o) { cg[b as usize] += 1; ng += 1; } }
        if ng * 4 < sel.len() * 3 { continue; }
        let (v, k) = cg.iter().enumerate().max_by_key(|(i, k)| (**k, std::cmp::Reverse(*i))).unwrap();
        if (*k as usize) != ng { continue; } // every present group row must agree
        let mut ct = 0usize;
        let mut nt = 0usize;
        for r in rows { if let Some(&b) = r.payload.get(o) { nt += 1; if b as usize == v { ct += 1; } } }
        let share = ct as f64 / nt.max(1) as f64;
        if share >= 0.5 { continue; }
        out.push(Same { off: o, value: v as u8, table_share: share, log10_p: ng as f64 * share.log10() });
    }
    Some((out, sel.len()))
}

fn run_same(archive: &crate::archive::Archive, opts: &Opts, out_dir: &Path) -> Result<(), String> {
    let group: BTreeSet<u32> = opts.same.iter().copied().filter(|&i| i != 0).collect();
    if group.len() < 3 { return Err("--find-same needs at least 3 ids that share the property.".to_string()); }
    println!("Same-value mode: {} id(s) that should share a rare byte. Score = log10(chance all of them match by luck); below -6 is notable.", group.len());
    struct Res { entry: u32, fi: u32, rows: usize, n: usize, s: Same }
    let mut all: Vec<Res> = Vec::new();
    let ents: Vec<_> = archive.entries().iter()
        .filter(|e| opts.any || (e.unknown1 >> 28) == 0x6)
        .filter(|e| opts.tables.is_empty() || opts.tables.contains(&e.file_index))
        .filter(|e| e.size >= (HDR + REC_HDR) as u32).collect();
    for (n, e) in ents.iter().enumerate() {
        let Ok(bytes) = archive.table(e.index) else { continue };
        let rows = parse_rows(&bytes);
        if opts.max_rows > 0 && rows.len() > opts.max_rows { continue; }
        if let Some((v, k)) = shared(&rows, &group, opts.window) {
            for s in v { if s.log10_p <= -4.0 { all.push(Res { entry: e.index, fi: e.file_index, rows: rows.len(), n: k, s }); } }
        }
        if (n + 1) % 200 == 0 { eprintln!("  ...{}/{} entries", n + 1, ents.len()); }
    }
    all.sort_by(|a, b| a.s.log10_p.partial_cmp(&b.s.log10_p).unwrap_or(std::cmp::Ordering::Equal).then(a.rows.cmp(&b.rows)).then(a.entry.cmp(&b.entry)).then(a.s.off.cmp(&b.s.off)));
    if all.is_empty() { println!("No byte is shared by the group and rare in its table."); return Ok(()); }
    println!("\n{:>3}  {:>7} {:>10} {:>7} {:>6}  {:>6} {:>5}  {:>10}  log10(chance)", "#", "entry", "FileIndex", "rows", "ids", "offset", "byte", "in table");
    for (i, r) in all.iter().take(opts.top.max(25)).enumerate() {
        println!("{:>3}  {:>7} 0x{:08X} {:>7} {:>6}  {:>6} 0x{:02X} {:>9.2}%  {:.1}", i + 1, r.entry, r.fi, r.rows, r.n, format!("+{}", r.s.off), r.s.value, r.s.table_share * 100.0, r.s.log10_p);
    }
    let dir = out_dir.join("find_ids");
    std::fs::create_dir_all(&dir).map_err(|e| format!("failed to create '{}': {}", dir.display(), e))?;
    let p = dir.join("same.csv");
    let io = |e: std::io::Error| format!("failed to write '{}': {}", p.display(), e);
    let mut f = BufWriter::new(File::create(&p).map_err(io)?);
    writeln!(f, "rank,entry,file_index,rows,ids_present,offset,byte,table_share,log10_chance").map_err(io)?;
    for (i, r) in all.iter().enumerate() {
        writeln!(f, "{},{},0x{:x},{},{},{},0x{:02X},{:.4},{:.2}", i + 1, r.entry, r.fi, r.rows, r.n, r.s.off, r.s.value, r.s.table_share, r.s.log10_p).map_err(io)?;
    }
    f.flush().map_err(io)?;
    println!("\nAll {} -> {}", all.len(), p.display());
    Ok(())
}

pub fn run(archive: &crate::archive::Archive, opts: &Opts, out_dir: &Path) -> Result<(), String> {
    if !opts.same.is_empty() { return run_same(archive, opts, out_dir); }
    if !opts.rows.is_empty() { return run_rows(archive, opts); }
    if !opts.yes.is_empty() { return run_compare(archive, opts, out_dir); }
    let targets = Targets::new(&opts.ids)?;
    let min_distinct = 3.min(targets.ids.len());

    let mut refs: Vec<TableRef> = archive.entries().iter()
        .filter(|e| opts.any || (e.unknown1 >> 28) == 0x6)
        .filter(|e| opts.tables.is_empty() || opts.tables.contains(&e.file_index))
        .filter(|e| e.size >= (HDR + REC_HDR) as u32)
        .map(|e| TableRef {
            entry: e.index, file_index: e.file_index, unk1: e.unknown1, arch: e.archive_index, offset: e.offset,
            name: archive.name(e).unwrap_or("").to_string(),
        })
        .collect();
    // archive/offset order = one sequential pass over each DAT
    refs.sort_by_key(|r| (r.arch, r.offset));

    println!("Searching {} candidate table entr{} for {} distinct id(s) ({}..{}), scan window {} bytes{}",
        refs.len(), if refs.len() == 1 { "y" } else { "ies" }, targets.ids.len(),
        targets.ids.iter().next().unwrap(), targets.ids.iter().next_back().unwrap(), opts.window,
        if opts.any { " [probing ALL manifest entries - slow]" } else { "" });
    if refs.is_empty() {
        return Err("no candidate tables (try --find-any, or check the --find-table values).".to_string());
    }

    let mut all: Vec<Cand> = Vec::new();
    let (mut tables, mut failed) = (0usize, 0usize);
    for (n, r) in refs.iter().enumerate() {
        let bytes = match archive.table(r.entry) {
            Ok(b) => b,
            Err(_) => { failed += 1; continue; } // not an EsoFileData table (or unreadable)
        };
        let rows = parse_rows(&bytes);
        if rows.is_empty() { continue; }
        tables += 1;
        let scan = scan_rows(&rows, &targets, opts.window);
        all.extend(candidates(r.entry, r.file_index, r.unk1, r.arch, &r.name, &scan, &targets, min_distinct));
        if (n + 1) % 200 == 0 {
            eprintln!("  ...{}/{} entries", n + 1, refs.len());
        }
    }
    println!("Scanned {} table(s); {} entr{} were not readable tables.", tables, failed, if failed == 1 { "y" } else { "ies" });

    rank(&mut all);
    if all.is_empty() {
        println!("\nNo column contains {} or more of the ids.", min_distinct);
        println!("Ideas: --find-any (tables outside category 0x6), --find-window 1024 (id deep inside the row),");
        println!("or the mapping goes through an intermediate id (look up a ranked table's other u32 columns next).");
        return Ok(());
    }

    println!("\nTop columns (cover = weighted share of your ids found in that ONE column; uniq ~1.0 = key-like):");
    println!("{:>3}  {:>7} {:>10} {:>4} {:>7}  {:<16} {:>9} {:>6} {:>6} {:>7} {:>8}  name",
        "#", "entry", "FileIndex", "arch", "rows", "column", "ids", "hits", "uniq", "idlike%", "chance");
    for (i, c) in all.iter().take(opts.top).enumerate() {
        println!("{:>3}  {:>7} 0x{:08X} {:>4} {:>7}  {:<16} {:>4}/{:<4} {:>6} {:>6.2} {:>7.1} {:>8.2}  {}  (cover {:.0}%)",
            i + 1, c.entry, c.file_index, c.arch, c.rows, c.col.label(), c.distinct, targets.ids.len(), c.hits,
            c.uniq, c.idlike, c.expected, c.name, c.cover * 100.0);
    }

    // Full dump + preview for the best few
    let dump_dir = out_dir.join("find_ids");
    let mut dumped: HashMap<(u32, Col), ()> = HashMap::new();
    for c in all.iter().take(opts.dump) {
        if dumped.insert((c.entry, c.col), ()).is_some() { continue; }
        let bytes = archive.table(c.entry)?;
        let rows = parse_rows(&bytes);
        println!("\n=== entry {} (FileIndex 0x{:x}), {} ===", c.entry, c.file_index, c.col.label());
        preview(&rows, c.col, &targets, 6);
        let path = dump_dir.join(format!("table_{}_fi{:x}_{}.csv", c.entry, c.file_index, c.col.short()));
        let n = dump_matches(&rows, c.col, &targets, &path)?;
        let present: BTreeSet<u32> = rows.iter().filter_map(|r| c.col.value(r)).filter(|v| targets.has(*v)).collect();
        let missing: Vec<String> = targets.ids.difference(&present).map(|i| i.to_string()).collect();
        println!("   {} row(s) -> {}", n, path.display());
        if !missing.is_empty() {
            let shown: Vec<&str> = missing.iter().take(40).map(String::as_str).collect();
            println!("   {} id(s) NOT in this column: {}{}", missing.len(), shown.join(","), if missing.len() > 40 { ",..." } else { "" });
        }
    }

    // All candidates, for spreadsheet sorting
    std::fs::create_dir_all(&dump_dir).map_err(|e| format!("failed to create '{}': {}", dump_dir.display(), e))?;
    let rp = dump_dir.join("ranking.csv");
    let io = |e: std::io::Error| format!("failed to write '{}': {}", rp.display(), e);
    let mut f = BufWriter::new(File::create(&rp).map_err(io)?);
    writeln!(f, "rank,entry,file_index,unk1,arch,rows,column,distinct_ids,hits,expected_by_chance,excess,cover,uniq,idlike_pct,name").map_err(io)?;
    for (i, c) in all.iter().enumerate() {
        writeln!(f, "{},{},0x{:x},0x{:08X},{},{},{},{},{},{:.3},{:.3},{:.4},{:.3},{:.1},{}", i + 1, c.entry, c.file_index, c.unk1, c.arch, c.rows,
            c.col.label(), c.distinct, c.hits, c.expected, c.excess, c.cover, c.uniq, c.idlike, csv_q(&c.name)).map_err(io)?;
    }
    f.flush().map_err(io)?;
    println!("\nFull ranking ({} columns) -> {}", all.len(), rp.display());
    Ok(())
}

// ------------------------------------------------------------------------------------------------
// Tests (no game files needed)
// ------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn build(rows: &[(u32, Vec<u8>)]) -> Vec<u8> {
        let mut d = Vec::new();
        for v in [0xFAFAEBEBu32, 23, rows.len() as u32, 6] { d.extend_from_slice(&v.to_be_bytes()); }
        for (i, (key, p)) in rows.iter().enumerate() {
            d.extend_from_slice(b"####");
            for v in [i as u32 + 1, p.len() as u32, p.len() as u32, p.len() as u32, *key, 0, p.len() as u32] {
                d.extend_from_slice(&v.to_be_bytes());
            }
            d.extend_from_slice(p);
        }
        d
    }

    fn payload(words: &[u32]) -> Vec<u8> { words.iter().flat_map(|w| w.to_be_bytes()).collect() }

    #[test]
    fn finds_the_reference_column_and_ignores_decoys() {
        let ids: Vec<u32> = (0..20).map(|i| 4700 + i * 7).collect();
        // table A: id at +8 (BE) in every row, plus a decoy small counter column at +0
        let a: Vec<(u32, Vec<u8>)> = ids.iter().enumerate().map(|(i, &id)| (100 + i as u32, payload(&[i as u32 % 5, 99999, id, 1, 2]))).collect();
        // table B: unrelated, two accidental hits
        let b: Vec<(u32, Vec<u8>)> = (0..50).map(|i| (i, payload(&[i, if i == 3 { ids[0] } else { 7 }, if i == 9 { ids[1] } else { 8 }]))).collect();
        let t = Targets::new(&ids).unwrap();
        let (da, db) = (build(&a), build(&b));
        let (ra, rb) = (parse_rows(&da), parse_rows(&db));
        assert_eq!(ra.len(), 20);
        let mut c = candidates(1, 0x10, 0x60000000, 0, "a", &scan_rows(&ra, &t, 64), &t, 3);
        c.extend(candidates(2, 0x11, 0x60000000, 0, "b", &scan_rows(&rb, &t, 64), &t, 3));
        rank(&mut c);
        assert_eq!(c[0].entry, 1);
        assert_eq!(c[0].col, Col { kind: Kind::Be, off: 8 });
        assert_eq!(c[0].distinct, 20);
        assert!((c[0].cover - 1.0).abs() < 1e-9);
        assert!((c[0].uniq - 1.0).abs() < 1e-9);
        // B has only 2 ids -> below min_distinct 3 -> not a candidate at all
        assert!(c.iter().all(|x| x.entry != 2));
    }

    #[test]
    fn key_column_and_little_endian_and_resync() {
        let ids: Vec<u32> = vec![2000, 2001, 2002, 2003];
        let rows: Vec<(u32, Vec<u8>)> = ids.iter().map(|&id| (id, vec![0u8; 12])).collect();
        let t = Targets::new(&ids).unwrap();
        let d = build(&rows);
        let r = parse_rows(&d);
        let c = candidates(1, 1, 0, 0, "", &scan_rows(&r, &t, 32), &t, 3);
        assert!(c.iter().any(|x| x.col.kind == Kind::Key && x.distinct == 4));

        let le: Vec<(u32, Vec<u8>)> = ids.iter().map(|&id| (0, id.to_le_bytes().to_vec())).collect();
        let d = build(&le);
        let r = parse_rows(&d);
        let c = candidates(1, 1, 0, 0, "", &scan_rows(&r, &t, 32), &t, 3);
        assert!(c.iter().any(|x| x.col == Col { kind: Kind::Le, off: 0 } && x.distinct == 4));

        // garbage between records: parser resyncs on the next ####
        let mut d = build(&rows);
        let cut = HDR + REC_HDR + 12; // end of record 1
        d.splice(cut..cut, [1u8, 2, 3, 4, 5]);
        assert_eq!(parse_rows(&d).len(), 4);
    }

    #[test]
    fn dense_key_columns_are_not_evidence() {
        let ids = vec![10u32, 20, 30, 40];
        let dense: Vec<(u32, Vec<u8>)> = (1..=100).map(|k| (k, vec![0u8; 8])).collect();
        let sparse: Vec<(u32, Vec<u8>)> = ids.iter().map(|&i| (i * 1000, payload(&[i, 5]))).collect();
        let t = Targets::new(&ids).unwrap();
        let (dd, sd) = (build(&dense), build(&sparse));
        let mut c = candidates(1, 1, 0, 0, "", &scan_rows(&parse_rows(&dd), &t, 16), &t, 3);
        c.extend(candidates(2, 2, 0, 0, "", &scan_rows(&parse_rows(&sd), &t, 16), &t, 3));
        rank(&mut c);
        assert_eq!(c[0].entry, 2);
        assert!(c.iter().filter(|x| x.entry == 1).all(|x| x.excess < 0.5));
        assert!(c.iter().all(|x| x.col.kind != Kind::Pos));
    }

    #[test]
    fn compare_finds_flag_byte() {
        let yes: Vec<u32> = (0..8).map(|i| 100 + i).collect();
        let no: Vec<u32> = (0..8).map(|i| 200 + i).collect();
        let mut rows = Vec::new();
        for (n, &k) in yes.iter().chain(no.iter()).enumerate() {
            let flag = if k < 200 { 0x01 } else { 0x00 };
            let mut p = k.to_be_bytes().to_vec();
            p.extend_from_slice(&[7, (n * 37 % 251) as u8, flag, 9]); // +4 const, +5 noise, +6 flag
            rows.push((k, p));
        }
        let d = build(&rows);
        let r = parse_rows(&d);
        let (ys, ns): (BTreeSet<u32>, BTreeSet<u32>) = (yes.iter().copied().collect(), no.iter().copied().collect());
        let (mut s, ny, nn) = separate(&r, &ys, &ns, 16).unwrap();
        assert_eq!((ny, nn), (8, 8));
        s.sort_by(|a, b| b.score().partial_cmp(&a.score()).unwrap());
        assert_eq!(s[0].off, 6);
        assert_eq!(s[0].value, 1);
        assert!((s[0].score() - 1.0).abs() < 1e-9);
        assert!(s.iter().find(|x| x.off == 4).unwrap().score() < 1e-9); // constant byte separates nothing
        // too few rows of one group -> no result
        let few: BTreeSet<u32> = [100, 5000, 5001, 5002].into_iter().collect();
        assert!(separate(&r, &few, &ns, 16).is_none());
    }

    #[test]
    fn clean_flag_beats_noise_and_rows_diff() {
        let yes: Vec<u32> = vec![1, 2, 3];
        let no: Vec<u32> = vec![10, 11, 12, 13];
        let mut rows = Vec::new();
        for (n, &k) in yes.iter().chain(no.iter()).enumerate() {
            let mut p = k.to_be_bytes().to_vec();
            p.extend_from_slice(&[(n * 41 % 250) as u8, if k < 10 { 5 } else { 0 }]); // +4 noise, +5 flag
            rows.push((k, p));
        }
        let d = build(&rows);
        let r = parse_rows(&d);
        let (ys, ns): (BTreeSet<u32>, BTreeSet<u32>) = (yes.iter().copied().collect(), no.iter().copied().collect());
        let (s, _, _) = separate(&r, &ys, &ns, 16).unwrap();
        let clean: Vec<usize> = s.iter().filter(|x| x.clean()).map(|x| x.off).collect();
        assert_eq!(clean, vec![5]);
        let sel: Vec<&Row> = r.iter().take(2).collect();
        let diff = differing_offsets(&sel, 16);
        assert!(diff.iter().any(|(o, _)| *o == 4));
        assert!(diff.iter().all(|(o, _)| *o != 5));
    }

    #[test]
    fn shared_rare_value_found() {
        let group: Vec<u32> = vec![10, 20, 30, 40];
        let mut rows = Vec::new();
        for k in 1..=200u32 {
            let g = group.contains(&k);
            // +4: rare marker 9 only for the group; +5: common value 0 for everyone; +6: noise
            rows.push((k, [&k.to_be_bytes()[..], &[if g { 9 } else { (k % 7) as u8 }, 0, (k % 251) as u8]].concat()));
        }
        let d = build(&rows);
        let r = parse_rows(&d);
        let gs: BTreeSet<u32> = group.iter().copied().collect();
        let (v, n) = shared(&r, &gs, 16).unwrap();
        assert_eq!(n, 4);
        assert!(v.iter().any(|s| s.off == 4 && s.value == 9 && s.log10_p < -6.0));
        assert!(v.iter().all(|s| s.off != 5)); // value 0 is in 100% of rows: not rare
    }

    #[test]
    fn legend_and_strings() {
        let dir = std::env::temp_dir().join("find_ids_test_legend.csv");
        std::fs::write(&dir, "layer_id,r,g,b,pixels,name\n0,1,2,3,4,\n4841,1,2,3,4,\n75,1,2,3,4,x\n").unwrap();
        assert_eq!(ids_from_legend(&dir).unwrap(), vec![0, 4841, 75]);
        assert_eq!(strings(b"\x00ab\x00water_lava\x01xyz1234", 4), vec!["water_lava".to_string(), "xyz1234".to_string()]);
    }
}