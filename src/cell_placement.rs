//! Parser for per-cell placement files (manifest type 0x15).
//!
//! Layout (all little-endian):
//!   u32 format_tag (0x17), u32 instance_count
//!   instance_count * 96-byte instances           (same record as placement.rs)
//!   "extra" section (variable; kept as an opaque blob, see notes)
//!   4 x [ RTRE header (36 bytes) + tree data ]
//!
//! RTRE header: magic "RTRE", 6 x i32 params (always 4,3,4,4,8,4 so far),
//!              i32 depth, i32 root_count.
//!   depth == 0 : `root_count` bare leaf entries follow (no node headers)
//!   depth  > 0 : `root_count` root nodes follow, each root has level == depth-1
//! Node : 6 x f32 bbox, i32 level, i32 child_count, then
//!        level == 0 -> child_count leaves (6 x f32 bbox, i32 object_id)
//!        level  > 0 -> child_count child nodes
use serde_json::{json, Value};
use std::io;
use std::path::Path;

const INSTANCE_SIZE: usize = 96;
const RTREE_HEADER_SIZE: usize = 36;
const MAGIC: &[u8; 4] = b"RTRE";
const MAX_DEPTH: usize = 32;

fn bad<T>(msg: impl Into<String>) -> io::Result<T> {
    Err(io::Error::new(io::ErrorKind::InvalidData, msg.into()))
}

struct Rd<'a> {
    b: &'a [u8],
    o: usize,
}

impl<'a> Rd<'a> {
    fn take(&mut self, n: usize) -> io::Result<&'a [u8]> {
        if self.o.checked_add(n).map_or(true, |e| e > self.b.len()) {
            return bad(format!("unexpected EOF at 0x{:x} (+{})", self.o, n));
        }
        let s = &self.b[self.o..self.o + n];
        self.o += n;
        Ok(s)
    }
    fn u16(&mut self) -> io::Result<u16> { Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap())) }
    fn u32(&mut self) -> io::Result<u32> { Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap())) }
    fn i32(&mut self) -> io::Result<i32> { Ok(i32::from_le_bytes(self.take(4)?.try_into().unwrap())) }
    fn f32(&mut self) -> io::Result<f32> { Ok(f32::from_le_bytes(self.take(4)?.try_into().unwrap())) }
    fn f32s<const N: usize>(&mut self) -> io::Result<[f32; N]> {
        let mut a = [0f32; N];
        for v in a.iter_mut() { *v = self.f32()?; }
        Ok(a)
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

fn parse_instance(r: &mut Rd) -> io::Result<Value> {
    let instance_id = r.u32()?;
    let reserved_1 = hex(r.take(12)?);
    let rot = r.f32s::<3>()?;
    let pos = r.f32s::<3>()?;
    let reserved_2a = hex(r.take(16)?);
    let extra_flag = r.u16()?;
    let reserved_2b = hex(r.take(10)?);
    let model_type_id = r.u32()?;
    let reserved_3 = hex(r.take(24)?);
    Ok(json!({
        "instance_id": instance_id,
        "reserved_1": reserved_1,
        "rot_x": rot[0], "rot_y": rot[1], "rot_z": rot[2],
        "pos_x": pos[0], "pos_y": pos[1], "pos_z": pos[2],
        "reserved_2a": reserved_2a,
        "extra_flag": extra_flag,
        "reserved_2b": reserved_2b,
        "model_type_id": model_type_id,
        "reserved_3": reserved_3,
    }))
}

fn parse_leaf(r: &mut Rd) -> io::Result<Value> {
    let bbox = r.f32s::<6>()?;
    let object_id = r.i32()?;
    Ok(json!({ "bbox": bbox, "object_id": object_id }))
}

fn parse_node(r: &mut Rd, depth: usize) -> io::Result<Value> {
    if depth > MAX_DEPTH {
        return bad("R-tree nesting too deep (corrupt file?)");
    }
    let bbox = r.f32s::<6>()?;
    let level = r.i32()?;
    let child_count = r.i32()?;
    if child_count < 0 || child_count as usize > r.b.len() {
        return bad(format!("implausible child_count {} at 0x{:x}", child_count, r.o));
    }
    if level == 0 {
        let mut leaves = Vec::with_capacity(child_count as usize);
        for _ in 0..child_count { leaves.push(parse_leaf(r)?); }
        Ok(json!({ "bbox": bbox, "level": level, "child_count": child_count, "leaves": leaves }))
    } else {
        let mut children = Vec::with_capacity(child_count as usize);
        for _ in 0..child_count { children.push(parse_node(r, depth + 1)?); }
        Ok(json!({ "bbox": bbox, "level": level, "child_count": child_count, "children": children }))
    }
}

fn parse_rtree_section(r: &mut Rd) -> io::Result<Value> {
    let start = r.o;
    let magic = r.take(4)?;
    if magic != MAGIC {
        return bad(format!("expected RTRE at 0x{:x}", start));
    }
    let mut params = [0i32; 6];
    for p in params.iter_mut() { *p = r.i32()?; }
    let tree_depth = r.i32()?;
    let root_count = r.i32()?;
    if root_count < 0 || root_count as usize > r.b.len() {
        return bad(format!("implausible root_count {} at 0x{:x}", root_count, start));
    }
    let mut out = json!({
        "offset": start,
        "params": params,
        "depth": tree_depth,
        "root_count": root_count,
    });
    if tree_depth == 0 {
        let mut leaves = Vec::with_capacity(root_count as usize);
        for _ in 0..root_count { leaves.push(parse_leaf(r)?); }
        out["leaves"] = Value::from(leaves);
    } else {
        let mut roots = Vec::with_capacity(root_count as usize);
        for _ in 0..root_count { roots.push(parse_node(r, 0)?); }
        out["roots"] = Value::from(roots);
    }
    debug_assert!(r.o >= start + RTREE_HEADER_SIZE);
    Ok(out)
}

fn find_first_rtre(buf: &[u8], from: usize) -> Option<usize> {
    let mut i = from;
    while i + RTREE_HEADER_SIZE <= buf.len() {
        if &buf[i..i + 4] == MAGIC && buf[i + 4..i + 8] == 4i32.to_le_bytes() {
            return Some(i);
        }
        i += 1;
    }
    None
}

pub fn parse_bytes(buf: &[u8]) -> io::Result<Value> {
    let buf = buf.to_vec();
    let mut r = Rd { b: &buf, o: 0 };

    let format_tag = r.u32()?;
    let instance_count = r.u32()? as usize;
    if instance_count.checked_mul(INSTANCE_SIZE).map_or(true, |n| n > buf.len()) {
        return bad(format!("instance_count {} does not fit in file", instance_count));
    }

    let mut instances = Vec::with_capacity(instance_count);
    for i in 0..instance_count {
        let mut v = parse_instance(&mut r)?;
        v["index"] = Value::from(i);
        instances.push(v);
    }

    let extra_start = r.o;
    let rtre = match find_first_rtre(&buf, extra_start) {
        Some(p) => p,
        None => return bad("no RTRE section found after instances"),
    };
    let extra = &buf[extra_start..rtre];
    r.o = rtre;

    let mut rtrees = Vec::with_capacity(4);
    for _ in 0..4 { rtrees.push(parse_rtree_section(&mut r)?); }

    let trailing = buf.len() - r.o;

    Ok(json!({
        "header": { "format_tag": format_tag, "instance_count": instance_count },
        "instances": instances,
        "extra_section": { "offset": extra_start, "length": extra.len(), "hex": hex(extra) },
        "rtrees": rtrees,
        "trailing_bytes": trailing,
    }))
}

pub fn parse_and_write_cell(input: &[u8], output: &Path) -> io::Result<()> {
    let v = parse_bytes(input)?;
    let s = serde_json::to_string_pretty(&v).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    if let Some(dir) = output.parent() { std::fs::create_dir_all(dir)?; }
    std::fs::write(output, s)
}