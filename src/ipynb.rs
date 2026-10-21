//! .ipynb (nbformat 4) to rhun's notebook body and back. rhun keeps every cell's and output's
//! JSON opaque (cell META, output KEEP): what rhun cannot show round-trips unchanged, and a file
//! Jupyter wrote is written back byte for byte when nothing changed (sorted keys, one-space
//! indent, a final line feed, as nbformat writes).

use rhun_ext::notebook::{Cell, CellKind, Item, Notebook, Output, OutputKind};
use serde_json::{json, Map, Value};

/// nbformat's multi-line strings: a string or a list of strings.
fn text_of(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(a) => a.iter().filter_map(|x| x.as_str()).collect(),
        _ => String::new(),
    }
}

/// A multi-line string as nbformat writes it: lines that keep their line feeds.
pub fn lines_of(s: &str) -> Value {
    let mut out = Vec::new();
    let mut rest = s;
    while !rest.is_empty() {
        match rest.find('\n') {
            Some(i) => {
                out.push(Value::String(rest[..=i].to_string()));
                rest = &rest[i + 1..];
            }
            None => {
                out.push(Value::String(rest.to_string()));
                rest = "";
            }
        }
    }
    Value::Array(out)
}

fn b64_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut acc = 0u32;
    let mut n = 0;
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' | b'\n' | b'\r' | b' ' | b'\t' => continue,
            _ => return None,
        };
        acc = acc << 6 | v as u32;
        n += 1;
        if n == 4 {
            out.extend_from_slice(&[(acc >> 16) as u8, (acc >> 8) as u8, acc as u8]);
            acc = 0;
            n = 0;
        }
    }
    match n {
        2 => out.push((acc >> 4) as u8),
        3 => out.extend_from_slice(&[(acc >> 10) as u8, (acc >> 2) as u8]),
        _ => {}
    }
    Some(out)
}

/// The items rhun can show for a MIME bundle, best first: images, then HTML and Markdown (kept
/// for rich rendering), then the plain text.
pub fn items_of(data: &Map<String, Value>, meta: Option<&Value>) -> Vec<Item> {
    let mut items = Vec::new();
    for (mime, kind) in [("image/png", "png"), ("image/jpeg", "jpeg"), ("image/gif", "gif")] {
        if let Some(v) = data.get(mime) {
            if let Some(bytes) = b64_decode(&text_of(v)) {
                let size = meta.and_then(|m| m.get(mime));
                let w = size.and_then(|s| s.get("width")).and_then(|v| v.as_u64()).unwrap_or(0).min(65536);
                let h = size.and_then(|s| s.get("height")).and_then(|v| v.as_u64()).unwrap_or(0).min(65536);
                items.push(Item::new(kind, &bytes).size(w as u32, h as u32));
            }
        }
    }
    if let Some(v) = data.get("text/html") {
        items.push(Item::new("html", text_of(v).as_bytes()));
    }
    if let Some(v) = data.get("text/markdown") {
        items.push(Item::new("markdown", text_of(v).as_bytes()));
    }
    if let Some(v) = data.get("text/plain") {
        items.push(Item::ansi(&text_of(v)));
    }
    if items.is_empty() {
        // only kinds rhun cannot show (widgets, JSON): name them
        let kinds: Vec<&str> = data.keys().map(|k| k.as_str()).collect();
        items.push(Item::text(&format!("[{}]", kinds.join(", "))));
    }
    items
}

/// An nbformat output as rhun's output: KEEP holds its JSON (a stream's is made again from its
/// text on save, because rhun appends to streams).
pub fn output_of(o: &Value) -> Output {
    let ty = o.get("output_type").and_then(|v| v.as_str()).unwrap_or("");
    let keep = serde_json::to_vec(o).unwrap_or_default();
    match ty {
        "stream" => {
            let kind = if o.get("name").and_then(|v| v.as_str()) == Some("stderr") {
                OutputKind::Stderr
            } else {
                OutputKind::Stdout
            };
            Output::new(kind, vec![Item::ansi(&text_of(o.get("text").unwrap_or(&Value::Null)))])
        }
        "error" => {
            let tb: Vec<String> = o
                .get("traceback")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
                .unwrap_or_default();
            let text = if tb.is_empty() {
                format!(
                    "{}: {}",
                    o.get("ename").and_then(|v| v.as_str()).unwrap_or(""),
                    o.get("evalue").and_then(|v| v.as_str()).unwrap_or("")
                )
            } else {
                tb.join("\n")
            };
            let mut out = Output::new(OutputKind::Error, vec![Item::ansi(&text)]);
            out.keep = keep;
            out
        }
        _ => {
            let kind = if ty == "execute_result" { OutputKind::Result } else { OutputKind::Display };
            let empty = Map::new();
            let data = o.get("data").and_then(|v| v.as_object()).unwrap_or(&empty);
            let mut out = Output::new(kind, items_of(data, o.get("metadata")));
            out.keep = keep;
            out
        }
    }
}

/// The notebook's language: its kernelspec's, else language_info's, else python.
pub fn language(nb: &Value) -> String {
    let md = nb.get("metadata");
    md.and_then(|m| m.get("kernelspec"))
        .and_then(|k| k.get("language"))
        .or_else(|| md.and_then(|m| m.get("language_info")).and_then(|l| l.get("name")))
        .and_then(|v| v.as_str())
        .unwrap_or("python")
        .replace(' ', "_")
}

/// Reads an .ipynb file into a notebook body. An empty file is a new notebook.
pub fn read(data: &[u8]) -> Result<Notebook, String> {
    if data.iter().all(|b| b.is_ascii_whitespace()) {
        let meta = json!({"metadata": {}, "nbformat": 4, "nbformat_minor": 5});
        return Ok(Notebook { meta: serde_json::to_vec(&meta).unwrap(), cells: Vec::new() });
    }
    let v: Value = serde_json::from_slice(data).map_err(|e| format!("not a notebook: {e}"))?;
    let obj = v.as_object().ok_or("not a notebook: the file is not a JSON object")?;
    let major = obj.get("nbformat").and_then(|v| v.as_u64()).unwrap_or(0);
    if major != 4 {
        return Err(format!("nbformat {major}: only nbformat 4 notebooks are read"));
    }
    let lang = language(&v);
    let mut meta = obj.clone();
    let cells_v = meta.remove("cells").unwrap_or(Value::Array(Vec::new()));
    let mut cells = Vec::new();
    for c in cells_v.as_array().map(|a| a.as_slice()).unwrap_or(&[]) {
        let Some(co) = c.as_object() else { continue };
        let ty = co.get("cell_type").and_then(|v| v.as_str()).unwrap_or("code");
        let source = text_of(co.get("source").unwrap_or(&Value::Null));
        let mut cmeta = co.clone();
        cmeta.remove("source");
        let mut cell = if ty == "code" {
            cmeta.remove("cell_type");
            let mut cell = Cell::code(&lang, source.as_bytes());
            cell.count = co.get("execution_count").and_then(|v| v.as_u64());
            cmeta.remove("execution_count");
            if let Some(outs) = cmeta.remove("outputs") {
                cell.outputs = outs.as_array().map(|a| a.iter().map(output_of).collect()).unwrap_or_default();
            }
            cell
        } else {
            // markdown, and raw cells (which keep their type in the metadata)
            if ty == "markdown" {
                cmeta.remove("cell_type");
            }
            Cell::markup(source.as_bytes())
        };
        cell.meta = serde_json::to_vec(&Value::Object(cmeta)).unwrap();
        cells.push(cell);
    }
    Ok(Notebook { meta: serde_json::to_vec(&Value::Object(meta)).unwrap(), cells })
}

/// An output as nbformat writes it: its KEEP JSON, or a stream made from its text.
pub fn output_json(o: &Output) -> Value {
    let text = o.items.first().map(|i| String::from_utf8_lossy(&i.data).into_owned()).unwrap_or_default();
    match o.kind {
        OutputKind::Stdout | OutputKind::Stderr => {
            let name = if o.kind == OutputKind::Stderr { "stderr" } else { "stdout" };
            json!({"name": name, "output_type": "stream", "text": lines_of(&text)})
        }
        _ => {
            if let Ok(v) = serde_json::from_slice::<Value>(&o.keep) {
                if v.is_object() {
                    return v;
                }
            }
            if o.kind == OutputKind::Error {
                json!({"ename": "Error", "evalue": text, "output_type": "error", "traceback": text.lines().collect::<Vec<_>>()})
            } else {
                let ty = if o.kind == OutputKind::Result { "execute_result" } else { "display_data" };
                let mut v = json!({"data": {"text/plain": lines_of(&text)}, "metadata": {}, "output_type": ty});
                if ty == "execute_result" {
                    v["execution_count"] = Value::Null;
                }
                v
            }
        }
    }
}

/// Writes a notebook body as an .ipynb file, with `patch` set in its metadata (the kernel's
/// kernelspec and language_info).
pub fn write(nb: &Notebook, patch: &[(&str, Value)]) -> Result<Vec<u8>, String> {
    let mut top: Map<String, Value> = match serde_json::from_slice(&nb.meta) {
        Ok(Value::Object(m)) => m,
        _ => Map::new(),
    };
    let md = top.entry("metadata").or_insert_with(|| json!({}));
    if !md.is_object() {
        *md = json!({});
    }
    for (k, v) in patch {
        md[*k] = v.clone();
    }
    top.entry("nbformat").or_insert(json!(4));
    top.entry("nbformat_minor").or_insert(json!(5));
    let minor = top.get("nbformat_minor").and_then(|v| v.as_u64()).unwrap_or(5);
    let metas: Vec<Map<String, Value>> = nb
        .cells
        .iter()
        .map(|c| match serde_json::from_slice(&c.meta) {
            Ok(Value::Object(m)) => m,
            _ => Map::new(),
        })
        .collect();
    let mut ids: Vec<String> = metas.iter().filter_map(|m| m.get("id").and_then(|v| v.as_str()).map(str::to_string)).collect();
    let mut cells = Vec::new();
    for (i, (c, mut co)) in nb.cells.iter().zip(metas).enumerate() {
        co.entry("metadata").or_insert_with(|| json!({}));
        if minor >= 5 && !co.contains_key("id") {
            // a cell made in rhun (whose META is empty): an id from its place, not taken
            let mut salt = 0u32;
            let id = loop {
                let id = format!("{:08x}", cell_hash(i, salt));
                if !ids.contains(&id) {
                    break id;
                }
                salt += 1;
            };
            ids.push(id.clone());
            co.insert("id".into(), Value::String(id));
        }
        let source = String::from_utf8_lossy(&c.source).into_owned();
        co.insert("source".into(), lines_of(&source));
        match c.kind {
            CellKind::Code => {
                co.insert("cell_type".into(), json!("code"));
                co.insert("execution_count".into(), c.count.map(Value::from).unwrap_or(Value::Null));
                co.insert("outputs".into(), Value::Array(c.outputs.iter().map(output_json).collect()));
            }
            CellKind::Markup => {
                let raw = co.get("cell_type").and_then(|v| v.as_str()) == Some("raw");
                if !raw {
                    co.insert("cell_type".into(), json!("markdown"));
                }
                co.remove("outputs");
                co.remove("execution_count");
            }
        }
        cells.push(Value::Object(co));
    }
    top.insert("cells".into(), Value::Array(cells));
    let mut out = Vec::new();
    let fmt = serde_json::ser::PrettyFormatter::with_indent(b" ");
    let mut ser = serde_json::Serializer::with_formatter(&mut out, fmt);
    serde::Serialize::serialize(&Value::Object(top), &mut ser).map_err(|e| e.to_string())?;
    out.push(b'\n');
    Ok(out)
}

/// An id for a cell rhun created (nbformat 4.5 wants one): the same for the same place, so
/// saving twice writes the same file.
fn cell_hash(i: usize, salt: u32) -> u32 {
    let mut h: u32 = 2166136261;
    for b in (i as u32).to_le_bytes().into_iter().chain(salt.to_le_bytes()) {
        h = (h ^ b as u32).wrapping_mul(16777619);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r##"{
 "cells": [
  {
   "cell_type": "markdown",
   "id": "a1",
   "metadata": {},
   "source": [
    "# Title\n",
    "text"
   ]
  },
  {
   "cell_type": "code",
   "execution_count": 2,
   "id": "b2",
   "metadata": {
    "tags": [
     "x"
    ]
   },
   "outputs": [
    {
     "name": "stdout",
     "output_type": "stream",
     "text": [
      "hi\n"
     ]
    },
    {
     "data": {
      "image/png": "iVBORw0KGgo=",
      "text/plain": [
       "<Figure>"
      ]
     },
     "metadata": {},
     "output_type": "display_data"
    },
    {
     "ename": "ValueError",
     "evalue": "bad",
     "output_type": "error",
     "traceback": [
      "\u001b[0;31mValueError\u001b[0m: bad"
     ]
    }
   ],
   "source": [
    "print('hi')"
   ]
  },
  {
   "cell_type": "raw",
   "id": "c3",
   "metadata": {},
   "source": []
  }
 ],
 "metadata": {
  "kernelspec": {
   "display_name": "Python 3",
   "language": "python",
   "name": "python3"
  }
 },
 "nbformat": 4,
 "nbformat_minor": 5
}
"##;

    #[test]
    fn round_trip_byte_for_byte() {
        let nb = read(SAMPLE.as_bytes()).unwrap();
        assert_eq!(nb.cells.len(), 3);
        assert_eq!(nb.cells[1].lang, "python");
        assert_eq!(nb.cells[1].count, Some(2));
        assert_eq!(nb.cells[1].outputs.len(), 3);
        assert_eq!(nb.cells[1].outputs[1].items[0].kind, "png");
        assert_eq!(nb.cells[1].outputs[1].items[0].data, b"\x89PNG\r\n\x1a\n");
        assert_eq!(nb.cells[2].kind, CellKind::Markup);
        let back = write(&Notebook::parse(&nb.encode()).unwrap(), &[]).unwrap();
        assert_eq!(String::from_utf8(back).unwrap(), SAMPLE);
    }

    #[test]
    fn new_and_edited() {
        let mut nb = read(b"").unwrap();
        nb.cells.push(Cell::code("python", b"x = 1\ny = 2"));
        let out = String::from_utf8(write(&nb, &[("language_info", json!({"name": "python"}))]).unwrap()).unwrap();
        assert!(out.contains("\"language_info\": {\n   \"name\": \"python\"\n  }"), "{out}");
        assert!(out.contains("\"id\": \""), "{out}");
        assert!(out.contains("\"source\": [\n    \"x = 1\\n\",\n    \"y = 2\"\n   ]"), "{out}");
        assert!(read(b"{\"nbformat\": 3}").is_err());
        assert!(read(b"not json").is_err());
    }
}
