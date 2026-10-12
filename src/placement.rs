use serde_json::{json, Value};
use std::fs;
use std::io;
use std::path::Path;

const INSTANCE_SIZE: usize = 96;
const RTREE_HEADER_SIZE: usize = 36;

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

fn read_u32(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(buf[off..off + 4].try_into().unwrap())
}
fn read_i32(buf: &[u8], off: usize) -> i32 {
    i32::from_le_bytes(buf[off..off + 4].try_into().unwrap())
}
fn read_u16(buf: &[u8], off: usize) -> u16 {
    u16::from_le_bytes(buf[off..off + 2].try_into().unwrap())
}
fn read_f32(buf: &[u8], off: usize) -> f32 {
    f32::from_le_bytes(buf[off..off + 4].try_into().unwrap())
}

fn parse_file_header(buf: &[u8], off: usize) -> Value {
    let format_tag = read_u32(buf, off);
    let instance_count = read_u32(buf, off + 4);
    json!({ "format_tag": format_tag, "instance_count": instance_count })
}

fn parse_instance(buf: &[u8], off: usize) -> Value {
    let mut o = off;

    let instance_id = read_u32(buf, o);
    o += 4;
    let reserved_1 = &buf[o..o + 12];
    o += 12;
    let rot_x = read_f32(buf, o);
    o += 4;
    let rot_y = read_f32(buf, o);
    o += 4;
    let rot_z = read_f32(buf, o);
    o += 4;
    let pos_x = read_f32(buf, o);
    o += 4;
    let pos_y = read_f32(buf, o);
    o += 4;
    let pos_z = read_f32(buf, o);
    o += 4;
    let reserved_2a = &buf[o..o + 16];
    o += 16;
    let extra_flag = read_u16(buf, o);
    o += 2;
    let reserved_2b = &buf[o..o + 10];
    o += 10;
    let model_type_id = read_u32(buf, o);
    o += 4;
    let reserved_3 = &buf[o..o + 24];

    json!({
        "instance_id": instance_id,
        "reserved_1": hex_encode(reserved_1),
        "rot_x": rot_x, "rot_y": rot_y, "rot_z": rot_z,
        "pos_x": pos_x, "pos_y": pos_y, "pos_z": pos_z,
        "reserved_2a": hex_encode(reserved_2a),
        "extra_flag": extra_flag,
        "reserved_2b": hex_encode(reserved_2b),
        "model_type_id": model_type_id,
        "reserved_3": hex_encode(reserved_3),
    })
}

fn parse_rtree_header(buf: &[u8], off: usize) -> Value {
    let magic = &buf[off..off + 4];
    let mut o = off + 4;
    let mut params = [0i32; 6];
    for p in params.iter_mut() {
        *p = read_i32(buf, o);
        o += 4;
    }
    let reserved = &buf[o..o + 8];
    json!({
        "magic": String::from_utf8_lossy(magic).into_owned(),
        "params": params,
        "reserved": hex_encode(reserved),
    })
}


fn parse_rtree_node(buf: &[u8], off: usize) -> (Value, usize) {
    let mut o = off;

    let mut bbox = [0f32; 6];
    for b in bbox.iter_mut() {
        *b = read_f32(buf, o);
        o += 4;
    }
    let level = read_i32(buf, o);
    o += 4;
    let child_count = read_i32(buf, o);
    o += 4;

    if level == 0 {
        let mut leaves = Vec::with_capacity(child_count.max(0) as usize);
        for _ in 0..child_count {
            let mut lbbox = [0f32; 6];
            for b in lbbox.iter_mut() {
                *b = read_f32(buf, o);
                o += 4;
            }
            let object_id = read_i32(buf, o);
            o += 4;
            leaves.push(json!({ "bbox": lbbox, "object_id": object_id }));
        }
        (
            json!({
                "bbox": bbox,
                "level": level,
                "child_count": child_count,
                "leaves": leaves,
            }),
            o,
        )
    } else {
        let mut children = Vec::with_capacity(child_count.max(0) as usize);
        for _ in 0..child_count {
            let (child, new_off) = parse_rtree_node(buf, o);
            o = new_off;
            children.push(child);
        }
        (
            json!({
                "bbox": bbox,
                "level": level,
                "child_count": child_count,
                "children": children,
            }),
            o,
        )
    }
}

pub fn parse_file(path: &Path) -> io::Result<Value> {
    let buf = fs::read(path)?;

    let header_val = parse_file_header(&buf, 0);
    let instance_count = header_val["instance_count"].as_u64().unwrap_or(0) as usize;

    let instance_array_start = 8usize;
    let mut instances = Vec::with_capacity(instance_count);
    let mut off = instance_array_start;
    for _ in 0..instance_count {
        instances.push(parse_instance(&buf, off));
        off += INSTANCE_SIZE;
    }

    let after_instances = instance_array_start + instance_count * INSTANCE_SIZE;
    let rtree_start = after_instances + 12;

    let mut rtree_headers = Vec::with_capacity(4);
    let mut off = rtree_start;
    for _ in 0..4 {
        rtree_headers.push(parse_rtree_header(&buf, off));
        off += RTREE_HEADER_SIZE;
    }

    let mut trees = Vec::new();
    while off < buf.len() {
        let (node, new_off) = parse_rtree_node(&buf, off);
        off = new_off;
        trees.push(node);
    }

    Ok(json!({
        "header": header_val,
        "instances": instances,
        "rtree_headers": rtree_headers,
        "trees": trees,
    }))
}


pub fn parse_and_write(input: &Path, output: &Path) -> io::Result<()> {
    let value = parse_file(input)?;
    let s = serde_json::to_string_pretty(&value)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    fs::write(output, s)?;
    Ok(())
}