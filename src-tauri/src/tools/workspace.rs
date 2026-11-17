use ignore::WalkBuilder;
use regex::RegexBuilder;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use uuid::Uuid;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileMutationResult {
    pub snapshot_id: String,
    pub path: String,
    pub existed_before: bool,
    pub added_lines: i64,
    pub removed_lines: i64,
    pub bytes_written: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrepMatch {
    pub file_path: String,
    pub line_number: usize,
    pub line_content: String,
    pub match_start: usize,
    pub match_end: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadFileResult {
    pub content: String,
    pub total_lines: usize,
    pub returned_lines: usize,
    pub truncated: bool,
}

pub const MAX_PATCH_DIFF_LINES: i64 = 5000;

fn is_reserved_windows_name(path: &Path) -> bool {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_uppercase();
    matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL"
            | "COM1" | "COM2" | "COM3" | "COM4" | "COM5" | "COM6" | "COM7" | "COM8" | "COM9"
            | "LPT1" | "LPT2" | "LPT3" | "LPT4" | "LPT5" | "LPT6" | "LPT7" | "LPT8" | "LPT9"
    )
}

pub fn validate_path(path_str: &str, allowed_roots: &[PathBuf]) -> Result<PathBuf, String> {
    if path_str.starts_with("\\\\") || path_str.starts_with("//") {
        return Err(format!("Access denied: UNC network path '{}' is not allowed.", path_str));
    }
    let raw = Path::new(path_str);
    if is_reserved_windows_name(raw) {
        return Err(format!("Invalid path: '{}' uses a reserved Windows device name.", path_str));
    }
    let p = if raw.is_absolute() {
        raw.to_path_buf()
    } else if let Some(base) = allowed_roots.first() {
        base.join(raw)
    } else {
        raw.to_path_buf()
    };

    let resolved = if p.exists() {
        p.canonicalize().map_err(|e| e.to_string())?
    } else {
        let mut ancestor = p.parent();
        let mut tail_parts: Vec<std::ffi::OsString> = Vec::new();
        if let Some(fname) = p.file_name() {
            tail_parts.push(fname.to_os_string());
        } else {
            return Err("Invalid file path".to_string());
        }
        while let Some(anc) = ancestor {
            if anc.exists() {
                let mut can = anc.canonicalize().map_err(|e| e.to_string())?;
                for part in tail_parts.iter().rev() {
                    can.push(part);
                }
                ancestor = None;
                let _ = ancestor;
                return ensure_within_roots(can, path_str, allowed_roots);
            }
            if let Some(name) = anc.file_name() {
                tail_parts.push(name.to_os_string());
            }
            ancestor = anc.parent();
        }
        p
    };

    ensure_within_roots(resolved, path_str, allowed_roots)
}

pub fn normalize_windows_path(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    let stripped = if s.starts_with(r"\\?\UNC\") {
        format!(r"\\{}", &s[8..])
    } else if s.starts_with(r"\\?\") {
        s[4..].to_string()
    } else {
        s.to_string()
    };
    PathBuf::from(stripped.replace('/', "\\"))
}

fn ensure_within_roots(resolved: PathBuf, raw_str: &str, allowed_roots: &[PathBuf]) -> Result<PathBuf, String> {
    let norm_resolved = normalize_windows_path(&resolved);
    if allowed_roots.is_empty() {
        return Ok(norm_resolved);
    }
    let res_lower = norm_resolved.to_string_lossy().to_lowercase();
    let res_prefix = res_lower.trim_end_matches('\\');

    for root in allowed_roots {
        let can_root = root.canonicalize().unwrap_or_else(|_| root.clone());
        let norm_root = normalize_windows_path(&can_root);
        let root_lower = norm_root.to_string_lossy().to_lowercase();
        let root_prefix = root_lower.trim_end_matches('\\');

        if res_lower == root_prefix || res_lower.starts_with(&format!("{}\\", root_prefix)) || res_lower.starts_with(&format!("{}/", root_prefix)) {
            return Ok(norm_resolved);
        }

        let is_bare_drive = res_prefix.len() <= 2 && res_prefix.ends_with(':');
        if !is_bare_drive && (root_prefix.starts_with(&format!("{}\\", res_prefix)) || root_prefix.starts_with(&format!("{}/", res_prefix))) {
            return Ok(norm_resolved);
        }
    }
    Err(format!("Access denied: path '{}' resolves outside allowed workspace root.", raw_str))
}

pub fn record_file_snapshot(
    conn: Option<&Connection>,
    chat_id: Option<&str>,
    file_path: &str,
    existed_before: bool,
    before_blob: Option<Vec<u8>>,
    after_blob: Option<Vec<u8>>,
    added_lines: i64,
    removed_lines: i64,
) -> Result<String, String> {
    let snapshot_id = Uuid::new_v4().to_string();
    let now = chrono::Utc::now().timestamp_millis();

    if let Some(c) = conn {
        c.execute(
            "INSERT INTO file_snapshots (id, chat_id, file_path, existed_before, before_blob, after_blob, added_lines, removed_lines, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            rusqlite::params![
                snapshot_id,
                chat_id,
                file_path,
                if existed_before { 1 } else { 0 },
                before_blob,
                after_blob,
                added_lines,
                removed_lines,
                now,
            ],
        )
        .map_err(|e| e.to_string())?;
    }
    Ok(snapshot_id)
}

fn compute_line_diff(old_text: &str, new_text: &str) -> (i64, i64) {
    let old_lines: Vec<&str> = old_text.lines().collect();
    let new_lines: Vec<&str> = new_text.lines().collect();

    let mut added = 0i64;
    let mut removed = 0i64;

    let mut i = 0;
    let mut j = 0;
    while i < old_lines.len() && j < new_lines.len() {
        if old_lines[i] == new_lines[j] {
            i += 1;
            j += 1;
        } else {
            let mut found_in_new = None;
            for offset in 1..20 {
                if j + offset < new_lines.len() && old_lines[i] == new_lines[j + offset] {
                    found_in_new = Some(offset);
                    break;
                }
            }
            if let Some(offset) = found_in_new {
                added += offset as i64;
                j += offset;
            } else {
                removed += 1;
                i += 1;
            }
        }
    }
    if i < old_lines.len() {
        removed += (old_lines.len() - i) as i64;
    }
    if j < new_lines.len() {
        added += (new_lines.len() - j) as i64;
    }
    (added, removed)
}

pub fn read_file(
    path_str: &str,
    offset: Option<usize>,
    limit: Option<usize>,
    allowed_roots: &[PathBuf],
) -> Result<ReadFileResult, String> {
    let target = validate_path(path_str, allowed_roots)?;
    if !target.exists() {
        return Err(format!("File does not exist: {}", path_str));
    }
    let content = fs::read_to_string(&target).map_err(|e| e.to_string())?;
    let all_lines: Vec<&str> = content.lines().collect();
    let total_lines = all_lines.len();

    let start = offset.unwrap_or(0);
    let count = limit.unwrap_or(300).min(400);

    let selected: Vec<String> = all_lines
        .into_iter()
        .skip(start)
        .take(count)
        .enumerate()
        .map(|(i, line)| format!("{:>4} | {}", start + i + 1, line))
        .collect();

    let returned_lines = selected.len();
    let truncated = (start + returned_lines) < total_lines;
    let mut body = selected.join("\n");
    if truncated {
        body.push_str(&format!(
            "\n[Showing lines {}-{} of {}. Use offset/limit to read further.]",
            start + 1,
            start + returned_lines,
            total_lines
        ));
    }

    Ok(ReadFileResult {
        content: body,
        total_lines,
        returned_lines,
        truncated,
    })
}

pub fn write_file(
    conn: Option<&Connection>,
    chat_id: Option<&str>,
    path_str: &str,
    new_content: &str,
    overwrite: bool,
    allowed_roots: &[PathBuf],
) -> Result<FileMutationResult, String> {
    let target = validate_path(path_str, allowed_roots)?;
    let existed_before = target.exists();

    let before_blob = if existed_before {
        if !overwrite {
            return Err(format!("File already exists and overwrite is false: {}", path_str));
        }
        Some(fs::read(&target).map_err(|e| e.to_string())?)
    } else {
        None
    };

    let old_text = before_blob
        .as_ref()
        .map(|b| String::from_utf8_lossy(b).to_string())
        .unwrap_or_default();
    let (added_lines, removed_lines) = compute_line_diff(&old_text, new_content);

    let after_blob = Some(new_content.as_bytes().to_vec());

    let snapshot_id = record_file_snapshot(
        conn,
        chat_id,
        &target.to_string_lossy(),
        existed_before,
        before_blob,
        after_blob,
        added_lines,
        removed_lines,
    )?;

    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }

    fs::write(&target, new_content.as_bytes()).map_err(|e| e.to_string())?;

    Ok(FileMutationResult {
        snapshot_id,
        path: target.to_string_lossy().to_string(),
        existed_before,
        added_lines,
        removed_lines,
        bytes_written: new_content.len(),
    })
}

pub fn replace_in_file(
    conn: Option<&Connection>,
    chat_id: Option<&str>,
    path_str: &str,
    target_str: &str,
    replacement: &str,
    replace_all: bool,
    allowed_roots: &[PathBuf],
) -> Result<FileMutationResult, String> {
    let target = validate_path(path_str, allowed_roots)?;
    if !target.exists() {
        return Err(format!("File not found: {}", path_str));
    }
    let before_blob = fs::read(&target).map_err(|e| e.to_string())?;
    let old_content = String::from_utf8(before_blob.clone())
        .map_err(|_| "File is not valid UTF-8".to_string())?;

    if !old_content.contains(target_str) {
        return Err(format!(
            "Target string not found in {}: {}",
            path_str, target_str
        ));
    }

    let new_content = if replace_all {
        old_content.replace(target_str, replacement)
    } else {
        old_content.replacen(target_str, replacement, 1)
    };

    let (added_lines, removed_lines) = compute_line_diff(&old_content, &new_content);
    let after_blob = Some(new_content.as_bytes().to_vec());

    let snapshot_id = record_file_snapshot(
        conn,
        chat_id,
        &target.to_string_lossy(),
        true,
        Some(before_blob),
        after_blob,
        added_lines,
        removed_lines,
    )?;

    fs::write(&target, new_content.as_bytes()).map_err(|e| e.to_string())?;

    Ok(FileMutationResult {
        snapshot_id,
        path: target.to_string_lossy().to_string(),
        existed_before: true,
        added_lines,
        removed_lines,
        bytes_written: new_content.len(),
    })
}

pub fn apply_unified_diff(
    conn: Option<&Connection>,
    chat_id: Option<&str>,
    path_str: &str,
    patch_text: &str,
    allowed_roots: &[PathBuf],
) -> Result<FileMutationResult, String> {
    let target = validate_path(path_str, allowed_roots)?;
    if !target.exists() {
        return Err(format!("File not found for diff application: {}", path_str));
    }

    let before_blob = fs::read(&target).map_err(|e| e.to_string())?;
    let old_text = String::from_utf8(before_blob.clone())
        .map_err(|_| "File is not valid UTF-8".to_string())?;

    let old_lines: Vec<&str> = old_text.lines().collect();
    let mut new_lines = old_lines.clone();

    let mut patch_lines = patch_text.lines().peekable();
    let mut current_idx = 0usize;
    let mut added_counter = 0i64;
    let mut removed_counter = 0i64;

    while let Some(line) = patch_lines.next() {
        if line.starts_with("@@") {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 2 {
                let minus_part = parts[1];
                if let Some(clean_part) = minus_part.strip_prefix('-') {
                    let subparts: Vec<&str> = clean_part.split(',').collect();
                    if let Ok(line_num) = subparts[0].parse::<usize>() {
                        current_idx = line_num.saturating_sub(1);
                    }
                }
            }
        } else if let Some(content) = line.strip_prefix('-') {
            if current_idx < new_lines.len() && new_lines[current_idx] == content {
                new_lines.remove(current_idx);
                removed_counter += 1;
            } else if let Some(found_pos) = new_lines.iter().skip(current_idx).position(|&l| l == content) {
                new_lines.remove(current_idx + found_pos);
                removed_counter += 1;
            }
        } else if let Some(content) = line.strip_prefix('+') {
            if current_idx <= new_lines.len() {
                new_lines.insert(current_idx, content);
                current_idx += 1;
                added_counter += 1;
            }
        } else if line.strip_prefix(' ').is_some() {
            current_idx += 1;
        }
    }

    let result_content = new_lines.join("\n");
    let after_blob = Some(result_content.as_bytes().to_vec());

    let snapshot_id = record_file_snapshot(
        conn,
        chat_id,
        &target.to_string_lossy(),
        true,
        Some(before_blob),
        after_blob,
        added_counter,
        removed_counter,
    )?;

    fs::write(&target, result_content.as_bytes()).map_err(|e| e.to_string())?;

    Ok(FileMutationResult {
        snapshot_id,
        path: target.to_string_lossy().to_string(),
        existed_before: true,
        added_lines: added_counter,
        removed_lines: removed_counter,
        bytes_written: result_content.len(),
    })
}

pub fn move_path(
    conn: Option<&Connection>,
    chat_id: Option<&str>,
    src_str: &str,
    dst_str: &str,
    allowed_roots: &[PathBuf],
) -> Result<FileMutationResult, String> {
    let src = validate_path(src_str, allowed_roots)?;
    let dst = validate_path(dst_str, allowed_roots)?;

    if !src.exists() {
        return Err(format!("Source path does not exist: {}", src_str));
    }

    let before_blob = if src.is_file() {
        Some(fs::read(&src).map_err(|e| e.to_string())?)
    } else {
        None
    };

    let existed_before = dst.exists();
    let old_text = before_blob
        .as_ref()
        .map(|b| String::from_utf8_lossy(b).to_string())
        .unwrap_or_default();
    let total_lines = old_text.lines().count() as i64;

    let snapshot_id = record_file_snapshot(
        conn,
        chat_id,
        &dst.to_string_lossy(),
        existed_before,
        before_blob.clone(),
        before_blob,
        total_lines,
        0,
    )?;

    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }

    fs::rename(&src, &dst).map_err(|e| e.to_string())?;

    Ok(FileMutationResult {
        snapshot_id,
        path: dst.to_string_lossy().to_string(),
        existed_before,
        added_lines: total_lines,
        removed_lines: 0,
        bytes_written: 0,
    })
}

pub fn delete_path(
    conn: Option<&Connection>,
    chat_id: Option<&str>,
    path_str: &str,
    recursive: bool,
    allowed_roots: &[PathBuf],
) -> Result<FileMutationResult, String> {
    let target = validate_path(path_str, allowed_roots)?;
    if !target.exists() {
        return Err(format!("Path does not exist: {}", path_str));
    }

    let before_blob = if target.is_file() {
        Some(fs::read(&target).map_err(|e| e.to_string())?)
    } else {
        None
    };

    let removed_lines = before_blob
        .as_ref()
        .map(|b| String::from_utf8_lossy(b).lines().count() as i64)
        .unwrap_or(0);

    let snapshot_id = record_file_snapshot(
        conn,
        chat_id,
        &target.to_string_lossy(),
        true,
        before_blob,
        None,
        0,
        removed_lines,
    )?;

    if target.is_dir() {
        if recursive {
            fs::remove_dir_all(&target).map_err(|e| e.to_string())?;
        } else {
            fs::remove_dir(&target).map_err(|e| e.to_string())?;
        }
    } else {
        fs::remove_file(&target).map_err(|e| e.to_string())?;
    }

    Ok(FileMutationResult {
        snapshot_id,
        path: target.to_string_lossy().to_string(),
        existed_before: true,
        added_lines: 0,
        removed_lines,
        bytes_written: 0,
    })
}

pub fn fast_grep(
    root_path_str: &str,
    pattern: &str,
    case_sensitive: bool,
    max_results: usize,
    allowed_roots: &[PathBuf],
) -> Result<Vec<GrepMatch>, String> {
    let root = validate_path(root_path_str, allowed_roots)?;
    let re = RegexBuilder::new(pattern)
        .case_insensitive(!case_sensitive)
        .build()
        .map_err(|e| e.to_string())?;

    let mut matches = Vec::new();
    let walker = WalkBuilder::new(&root)
        .hidden(true)
        .git_ignore(true)
        .build();

    for entry in walker.filter_map(Result::ok) {
        if entry.file_type().map(|f| f.is_file()).unwrap_or(false) {
            let path = entry.path();
            if let Ok(file_content) = fs::read_to_string(path) {
                for (idx, line) in file_content.lines().enumerate() {
                    if let Some(m) = re.find(line) {
                        matches.push(GrepMatch {
                            file_path: path.to_string_lossy().to_string(),
                            line_number: idx + 1,
                            line_content: line.to_string(),
                            match_start: m.start(),
                            match_end: m.end(),
                        });
                        if matches.len() >= max_results {
                            return Ok(matches);
                        }
                    }
                }
            }
        }
    }

    Ok(matches)
}

pub fn find_files(
    root_path_str: &str,
    pattern: &str,
    max_results: usize,
    allowed_roots: &[PathBuf],
) -> Result<Vec<String>, String> {
    let root = validate_path(root_path_str, allowed_roots)?;
    let re = RegexBuilder::new(pattern)
        .case_insensitive(true)
        .build()
        .map_err(|e| e.to_string())?;

    let mut results = Vec::new();
    let walker = WalkBuilder::new(&root)
        .hidden(true)
        .git_ignore(true)
        .build();

    for entry in walker.filter_map(Result::ok) {
        let path = entry.path();
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        if re.is_match(&name) || re.is_match(&path.to_string_lossy()) {
            results.push(path.to_string_lossy().to_string());
            if results.len() >= max_results {
                break;
            }
        }
    }

    Ok(results)
}

pub fn pack_zip(
    source_dir_str: &str,
    output_zip_str: &str,
    allowed_roots: &[PathBuf],
) -> Result<u64, String> {
    let source_dir = validate_path(source_dir_str, allowed_roots)?;
    let output_zip = validate_path(output_zip_str, allowed_roots)?;

    if !source_dir.is_dir() {
        return Err(format!("Source must be a directory: {}", source_dir_str));
    }

    if let Some(parent) = output_zip.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }

    let file = File::create(&output_zip).map_err(|e| e.to_string())?;
    let mut zip = ZipWriter::new(file);
    let options = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .unix_permissions(0o755);

    let walker = WalkBuilder::new(&source_dir)
        .hidden(false)
        .git_ignore(false)
        .build();

    let mut total_bytes = 0u64;

    for entry in walker.filter_map(Result::ok) {
        let path = entry.path();
        if path == source_dir {
            continue;
        }

        let rel_path = path
            .strip_prefix(&source_dir)
            .map_err(|e| e.to_string())?;
        let name = rel_path
            .to_str()
            .ok_or_else(|| "Invalid UTF-8 in path".to_string())?
            .replace('\\', "/");

        if path.is_file() {
            zip.start_file(name, options).map_err(|e| e.to_string())?;
            let mut f = File::open(path).map_err(|e| e.to_string())?;
            let mut buf = Vec::new();
            f.read_to_end(&mut buf).map_err(|e| e.to_string())?;
            zip.write_all(&buf).map_err(|e| e.to_string())?;
            total_bytes += buf.len() as u64;
        } else if path.is_dir() {
            let dir_name = if name.ends_with('/') {
                name
            } else {
                format!("{}/", name)
            };
            zip.add_directory(dir_name, options)
                .map_err(|e| e.to_string())?;
        }
    }

    zip.finish().map_err(|e| e.to_string())?;
    Ok(total_bytes)
}

pub fn unpack_zip(
    zip_path_str: &str,
    destination_dir_str: &str,
    allowed_roots: &[PathBuf],
) -> Result<usize, String> {
    let zip_path = validate_path(zip_path_str, allowed_roots)?;
    let dest_dir = validate_path(destination_dir_str, allowed_roots)?;

    let file = File::open(&zip_path).map_err(|e| e.to_string())?;
    let mut archive = ZipArchive::new(file).map_err(|e| e.to_string())?;

    fs::create_dir_all(&dest_dir).map_err(|e| e.to_string())?;
    let mut extracted_count = 0usize;

    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).map_err(|e| e.to_string())?;
        let raw_name = entry.name().to_string();

        if raw_name.contains("..") || raw_name.starts_with('/') || raw_name.starts_with('\\') {
            return Err(format!("Unsafe path in archive: {}", raw_name));
        }

        let outpath = dest_dir.join(raw_name);

        if entry.is_dir() {
            fs::create_dir_all(&outpath).map_err(|e| e.to_string())?;
        } else {
            if let Some(p) = outpath.parent() {
                if !p.exists() {
                    fs::create_dir_all(p).map_err(|e| e.to_string())?;
                }
            }
            let mut outfile = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&outpath)
                .map_err(|e| e.to_string())?;
            std::io::copy(&mut entry, &mut outfile).map_err(|e| e.to_string())?;
            extracted_count += 1;
        }
    }

    Ok(extracted_count)
}


#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EditOp {
    pub old_text: String,
    pub new_text: String,
}

fn check_bracket_balance(content: &str) -> Option<String> {
    let mut parens = 0i32;
    let mut braces = 0i32;
    let mut brackets = 0i32;
    let mut in_str: Option<char> = None;
    let mut escaped = false;
    for ch in content.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' && in_str.is_some() {
            escaped = true;
            continue;
        }
        if let Some(q) = in_str {
            if ch == q {
                in_str = None;
            }
            continue;
        }
        match ch {
            '"' | '\'' => in_str = Some(ch),
            '(' => parens += 1,
            ')' => parens -= 1,
            '{' => braces += 1,
            '}' => braces -= 1,
            '[' => brackets += 1,
            ']' => brackets -= 1,
            _ => {}
        }
    }
    if parens != 0 || braces != 0 || brackets != 0 {
        Some(format!(
            "Syntax warning: unbalanced delimiters after edit (parens: {:+}, braces: {:+}, brackets: {:+})",
            parens, braces, brackets
        ))
    } else {
        None
    }
}

pub fn atomic_write_bytes(target: &Path, bytes: &[u8]) -> Result<(), String> {
    let write_target = if fs::symlink_metadata(target)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
    {
        fs::canonicalize(target).map_err(|e| format!("Failed to resolve symlink target: {}", e))?
    } else {
        target.to_path_buf()
    };

    if let Some(parent) = write_target.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }

    let existing_perms = fs::metadata(&write_target).ok().map(|m| m.permissions());

    let tmp_name = format!(
        ".{}.tmp.{}",
        write_target.file_name().and_then(|n| n.to_str()).unwrap_or("file"),
        Uuid::new_v4()
    );
    let tmp_path = write_target.with_file_name(tmp_name);

    {
        let mut f = File::create(&tmp_path).map_err(|e| e.to_string())?;
        if let Err(e) = f.write_all(bytes).and_then(|_| f.sync_all()) {
            let _ = fs::remove_file(&tmp_path);
            return Err(e.to_string());
        }
    }

    if let Some(perms) = existing_perms {
        let _ = fs::set_permissions(&tmp_path, perms);
    }

    fs::rename(&tmp_path, &write_target).map_err(|e| {
        let _ = fs::remove_file(&tmp_path);
        e.to_string()
    })
}

pub fn read_file_smart(
    path_str: &str,
    offset: Option<usize>,
    limit: Option<usize>,
    allowed_roots: &[PathBuf],
    guard: &crate::tools::safety::FileReadGuard,
) -> Result<serde_json::Value, String> {
    let target = validate_path(path_str, allowed_roots)?;
    crate::tools::safety::SecretShield::check_path(&target)?;
    if !target.exists() {
        return Err(format!("File does not exist: {}", path_str));
    }

    let ext = target
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    let raw_bytes = fs::read(&target).map_err(|e| e.to_string())?;
    guard.record_read(&target, &raw_bytes);

    if matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "webp" | "gif") {
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&raw_bytes);
        let mime = if ext == "jpg" { "jpeg" } else { ext.as_str() };
        return Ok(serde_json::json!({
            "kind": "image",
            "path": target.to_string_lossy(),
            "bytes": raw_bytes.len(),
            "dataUrl": format!("data:image/{};base64,{}", mime, b64),
        }));
    }

    let text = String::from_utf8_lossy(&raw_bytes);
    let all_lines: Vec<&str> = text.lines().collect();
    let total_lines = all_lines.len();
    let start = offset.unwrap_or(0);
    let count = limit.unwrap_or(350).min(400);

    if total_lines > 0 && start >= total_lines {
        return Ok(serde_json::json!({
            "kind": "text",
            "path": target.to_string_lossy(),
            "totalLines": total_lines,
            "returnedLines": 0,
            "truncated": false,
            "content": format!("[Offset {} is past EOF (totalLines={}).]", start, total_lines),
        }));
    }

    let mut selected: Vec<String> = Vec::new();
    let mut line_chars_truncated = false;
    let mut total_chars = 0usize;
    let max_body_chars = 20_000usize;
    let max_line_chars = 8_000usize;

    for (i, line) in all_lines.into_iter().skip(start).take(count).enumerate() {
        let char_cnt = line.chars().count();
        let line_text = if char_cnt > max_line_chars {
            line_chars_truncated = true;
            let end_idx = line.char_indices().nth(max_line_chars).map(|(p, _)| p).unwrap_or(line.len());
            format!("{}... [line truncated at {} of {} chars]", &line[..end_idx], max_line_chars, char_cnt)
        } else {
            line.to_string()
        };
        let formatted = format!("L{}: {}", start + i + 1, line_text);
        if total_chars + formatted.len() > max_body_chars && !selected.is_empty() {
            line_chars_truncated = true;
            break;
        }
        total_chars += formatted.len() + 1;
        selected.push(formatted);
    }

    let returned = selected.len();
    let truncated = line_chars_truncated || ((start + returned) < total_lines);
    let mut body = selected.join("\n");
    if truncated && returned > 0 {
        body.push_str(&format!(
            "\n[Showing lines L{}-L{} of {} (truncated={}). Use offset/limit to read further.]",
            start + 1,
            start + returned,
            total_lines,
            truncated
        ));
    }

    Ok(serde_json::json!({
        "kind": "text",
        "path": target.to_string_lossy(),
        "totalLines": total_lines,
        "returnedLines": returned,
        "truncated": truncated,
        "content": body,
    }))
}

fn apply_single_edit_cascade(source: &str, old_text: &str, new_text: &str, replace_all: bool) -> Result<(String, usize), String> {
    if old_text.is_empty() {
        return Err("old_text cannot be empty".to_string());
    }
    if source.contains(old_text) {
        let match_count = source.matches(old_text).count();
        if !replace_all && match_count > 1 {
            return Err(format!(
                "Ambiguous edit: old_text matches {} locations in file. Provide more unique context or set replace_all=true.",
                match_count
            ));
        }
        let out = if replace_all {
            source.replace(old_text, new_text)
        } else {
            source.replacen(old_text, new_text, 1)
        };
        return Ok((out, match_count));
    }

    let is_crlf = source.contains("\r\n");
    let norm_src = source.replace("\r\n", "\n");
    let norm_old = old_text.replace("\r\n", "\n");
    let norm_new = new_text.replace("\r\n", "\n");
    if norm_src.contains(&norm_old) {
        let match_count = norm_src.matches(&norm_old).count();
        if !replace_all && match_count > 1 {
            return Err(format!(
                "Ambiguous edit: old_text matches {} locations in file. Provide more unique context or set replace_all=true.",
                match_count
            ));
        }
        let out = if replace_all {
            norm_src.replace(&norm_old, &norm_new)
        } else {
            norm_src.replacen(&norm_old, &norm_new, 1)
        };
        let restored = if is_crlf { out.replace('\n', "\r\n") } else { out };
        return Ok((restored, match_count));
    }

    let clean_s = norm_src.replace('\u{FEFF}', "").replace('\u{00A0}', " ").replace('\u{200B}', "");
    let clean_o = norm_old.replace('\u{FEFF}', "").replace('\u{00A0}', " ").replace('\u{200B}', "");
    if clean_s.contains(&clean_o) {
        let match_count = clean_s.matches(&clean_o).count();
        if !replace_all && match_count > 1 {
            return Err(format!(
                "Ambiguous edit: old_text matches {} locations in file. Provide more unique context or set replace_all=true.",
                match_count
            ));
        }
        let out = if replace_all {
            clean_s.replace(&clean_o, &norm_new)
        } else {
            clean_s.replacen(&clean_o, &norm_new, 1)
        };
        let restored = if is_crlf { out.replace('\n', "\r\n") } else { out };
        return Ok((restored, match_count));
    }

    let esc_o = norm_old.replace("\n", "\\n");
    if esc_o != norm_old && norm_src.contains(&esc_o) {
        let match_count = norm_src.matches(&esc_o).count();
        if !replace_all && match_count > 1 {
            return Err(format!(
                "Ambiguous edit: old_text matches {} locations in file. Provide more unique context or set replace_all=true.",
                match_count
            ));
        }
        let esc_n = norm_new.replace("\n", "\\n");
        let out = if replace_all {
            norm_src.replace(&esc_o, &esc_n)
        } else {
            norm_src.replacen(&esc_o, &esc_n, 1)
        };
        let restored = if is_crlf { out.replace('\n', "\r\n") } else { out };
        return Ok((restored, match_count));
    }

    let trim_lines = |s: &str| -> Vec<String> {
        s.lines().map(|l| l.trim_end().to_string()).collect()
    };
    let src_lines = trim_lines(&norm_src);
    let old_lines = trim_lines(&norm_old);
    if !old_lines.is_empty() && src_lines.len() >= old_lines.len() {
        let mut hits = Vec::new();
        for i in 0..=src_lines.len() - old_lines.len() {
            if src_lines[i..i + old_lines.len()] == old_lines[..] {
                hits.push(i);
            }
        }
        if hits.len() > 1 && !replace_all {
            return Err(format!("Ambiguous edit: old_text matches {} locations in file. Provide more unique context or set replace_all=true.", hits.len()));
        }
        if let Some(&i) = hits.first() {
            let mut out_lines: Vec<String> = norm_src.lines().map(|l| l.to_string()).collect();
            let new_lines: Vec<String> = norm_new.lines().map(|l| l.to_string()).collect();
            out_lines.splice(i..i + old_lines.len(), new_lines);
            return Ok((out_lines.join("\n"), 1));
        }
    }

    let trim_all = |s: &str| -> Vec<String> {
        s.lines().map(|l| l.trim().to_string()).collect()
    };
    let src_trim = trim_all(&norm_src);
    let old_trim = trim_all(&norm_old);
    if !old_trim.is_empty() && src_trim.len() >= old_trim.len() {
        for i in 0..=src_trim.len() - old_trim.len() {
            if src_trim[i..i + old_trim.len()] == old_trim[..] {
                let mut out_lines: Vec<String> = norm_src.lines().map(|l| l.to_string()).collect();
                let base_indent: String = out_lines[i]
                    .chars()
                    .take_while(|c| c.is_whitespace())
                    .collect();
                let old_first_indent = norm_old
                    .lines()
                    .next()
                    .map(|l| l.chars().take_while(|c| c.is_whitespace()).count())
                    .unwrap_or(0);
                let new_lines: Vec<String> = norm_new
                    .lines()
                    .map(|l| {
                        let stripped: String = if l.chars().take_while(|c| c.is_whitespace()).count() >= old_first_indent {
                            l.chars().skip(old_first_indent).collect()
                        } else {
                            l.trim_start().to_string()
                        };
                        format!("{}{}", base_indent, stripped)
                    })
                    .collect();
                out_lines.splice(i..i + old_lines.len(), new_lines);
                return Ok((out_lines.join("\n"), 1));
            }
        }
    }

    let strip_ln_prefix = |s: &str| -> String {
        let trimmed = s.trim_start();
        if let Some(pos) = trimmed.find('|') {
            if pos > 0 && pos <= 6 && trimmed[..pos].trim().chars().all(|c| c.is_ascii_digit()) {
                return trimmed[pos + 1..].trim_start_matches(' ').to_string();
            }
        }
        s.to_string()
    };
    let collapse_ws = |s: &str| -> Vec<String> {
        s.lines()
            .map(|l| strip_ln_prefix(l).split_whitespace().collect::<Vec<_>>().join(" "))
            .collect()
    };
    let src_ws = collapse_ws(&norm_src);
    let old_ws = collapse_ws(&norm_old);
    if !old_ws.is_empty() && old_ws.iter().any(|l| !l.is_empty()) && src_ws.len() >= old_ws.len() {
        for i in 0..=src_ws.len() - old_ws.len() {
            if src_ws[i..i + old_ws.len()] == old_ws[..] {
                let mut out_lines: Vec<String> = norm_src.lines().map(|l| l.to_string()).collect();
                let base_indent: String = out_lines[i]
                    .chars()
                    .take_while(|c| c.is_whitespace())
                    .collect();
                let old_first_indent = norm_old
                    .lines()
                    .next()
                    .map(|l| strip_ln_prefix(l).chars().take_while(|c| c.is_whitespace()).count())
                    .unwrap_or(0);
                let new_lines: Vec<String> = norm_new
                    .lines()
                    .map(|l| {
                        let clean_l = strip_ln_prefix(l);
                        let stripped: String = if clean_l.chars().take_while(|c| c.is_whitespace()).count() >= old_first_indent {
                            clean_l.chars().skip(old_first_indent).collect()
                        } else {
                            clean_l.trim_start().to_string()
                        };
                        format!("{}{}", base_indent, stripped)
                    })
                    .collect();
                out_lines.splice(i..i + old_ws.len(), new_lines);
                return Ok((out_lines.join("\n"), 1));
            }
        }
    }

    Err(format!("old_text not found in file (even after whitespace/indent cascade): '{}'. Call read_file around the target lines and pass the exact substring.", old_text.lines().next().unwrap_or("")))
}

pub fn edit_file_cascade(
    conn: Option<&Connection>,
    chat_id: Option<&str>,
    path_str: &str,
    edits: &[EditOp],
    replace_all: bool,
    allowed_roots: &[PathBuf],
    guard: &crate::tools::safety::FileReadGuard,
) -> Result<serde_json::Value, String> {
    let target = validate_path(path_str, allowed_roots)?;
    crate::tools::safety::SecretShield::check_path(&target)?;
    if !target.exists() {
        return Err(format!("File does not exist: {}", path_str));
    }
    guard.verify_before_write(&target)?;

    let before_bytes = fs::read(&target).map_err(|e| e.to_string())?;
    let before_str = String::from_utf8_lossy(&before_bytes).to_string();
    let mut current = before_str.clone();
    let mut applied = 0usize;

    for op in edits {
        let (next, cnt) = apply_single_edit_cascade(&current, &op.old_text, &op.new_text, replace_all)?;
        current = next;
        applied += cnt;
    }

    let (added, removed) = compute_line_diff(&before_str, &current);
    if added + removed > MAX_PATCH_DIFF_LINES {
        return Err(format!(
            "Patch diff exceeds safety limit ({} > {} lines). Split into smaller edits or use write_file.",
            added + removed,
            MAX_PATCH_DIFF_LINES
        ));
    }

    let before_hash = crate::tools::safety::FileReadGuard::hash_bytes(&before_bytes);
    let after_bytes = current.as_bytes();
    let after_hash = crate::tools::safety::FileReadGuard::hash_bytes(after_bytes);
    let truncated_or_unsaved = (after_bytes.len() as u64) > crate::tools::blob_store::MAX_BLOB_BYTES;

    atomic_write_bytes(&target, after_bytes)?;
    guard.record_read(&target, after_bytes);

    let snapshot_id = record_file_snapshot(
        conn,
        chat_id,
        &target.to_string_lossy(),
        true,
        if truncated_or_unsaved { None } else { Some(before_bytes) },
        if truncated_or_unsaved { None } else { Some(after_bytes.to_vec()) },
        added,
        removed,
    )?;

    let ext = target.extension().and_then(|e| e.to_str()).unwrap_or("");
    let syntax_warning = if matches!(ext, "rs" | "ts" | "js" | "svelte" | "json" | "cpp" | "h" | "java" | "cs") {
        check_bracket_balance(&current)
    } else {
        None
    };

    Ok(serde_json::json!({
        "path": target.to_string_lossy(),
        "snapshotId": snapshot_id,
        "editsApplied": applied,
        "addedLines": added,
        "removedLines": removed,
        "beforeHash": before_hash,
        "afterHash": after_hash,
        "truncatedOrUnsaved": truncated_or_unsaved,
        "syntaxWarning": syntax_warning,
    }))
}

pub fn apply_unified_diff_safe(
    conn: Option<&Connection>,
    chat_id: Option<&str>,
    path_str: &str,
    patch_text: &str,
    allowed_roots: &[PathBuf],
    guard: &crate::tools::safety::FileReadGuard,
) -> Result<serde_json::Value, String> {
    let target = validate_path(path_str, allowed_roots)?;
    crate::tools::safety::SecretShield::check_path(&target)?;
    if !target.exists() {
        return Err(format!("File not found for patch: {}", path_str));
    }
    guard.verify_before_write(&target)?;

    let before_bytes = fs::read(&target).map_err(|e| e.to_string())?;
    let before_str = String::from_utf8(before_bytes.clone())
        .map_err(|_| "File is not valid UTF-8".to_string())?;
    let before_hash = crate::tools::safety::FileReadGuard::hash_bytes(&before_bytes);

    let old_lines: Vec<&str> = before_str.lines().collect();
    let mut new_lines: Vec<String> = old_lines.iter().map(|l| (*l).to_string()).collect();
    let mut line_offset: isize = 0;

    let patch_lines: Vec<&str> = patch_text.lines().collect();
    let mut idx = 0usize;
    while idx < patch_lines.len() {
        let line = patch_lines[idx];
        if line.starts_with("--- ") || line.starts_with("+++ ") || line.starts_with("diff ") || line.starts_with("index ") {
            idx += 1;
            continue;
        }
        if line.starts_with("@@") {
            let parts: Vec<&str> = line.split_whitespace().collect();
            let old_start = parts
                .get(1)
                .and_then(|p| p.strip_prefix('-'))
                .and_then(|p| p.split(',').next())
                .and_then(|n| n.parse::<usize>().ok())
                .ok_or_else(|| format!("Invalid hunk header: {}", line))?;

            let mut cursor = (old_start.saturating_sub(1) as isize + line_offset).max(0) as usize;
            idx += 1;
            while idx < patch_lines.len() && !patch_lines[idx].starts_with("@@") {
                let hline = patch_lines[idx];
                if let Some(rest) = hline.strip_prefix(' ') {
                    if cursor >= new_lines.len() || new_lines[cursor] != rest {
                        return Err(format!("Patch context mismatch at line {}", cursor + 1));
                    }
                    cursor += 1;
                } else if let Some(rest) = hline.strip_prefix('-') {
                    if cursor >= new_lines.len() || new_lines[cursor] != rest {
                        return Err(format!("Patch removal mismatch at line {}", cursor + 1));
                    }
                    new_lines.remove(cursor);
                    line_offset -= 1;
                } else if let Some(rest) = hline.strip_prefix('+') {
                    if cursor <= new_lines.len() {
                        new_lines.insert(cursor, rest.to_string());
                        cursor += 1;
                        line_offset += 1;
                    }
                } else if hline.starts_with("\\ No newline") {
                } else if !hline.is_empty() {
                    return Err(format!("Invalid patch line: {}", hline));
                }
                idx += 1;
            }
            continue;
        }
        idx += 1;
    }

    let mut current = new_lines.join("\n");
    if before_str.ends_with('\n') && !current.ends_with('\n') {
        current.push('\n');
    }

    let (added, removed) = compute_line_diff(&before_str, &current);
    if added + removed > MAX_PATCH_DIFF_LINES {
        return Err(format!(
            "Patch diff exceeds safety limit ({} > {} lines).",
            added + removed,
            MAX_PATCH_DIFF_LINES
        ));
    }

    let after_bytes = current.as_bytes();
    let after_hash = crate::tools::safety::FileReadGuard::hash_bytes(after_bytes);
    let truncated_or_unsaved = (after_bytes.len() as u64) > crate::tools::blob_store::MAX_BLOB_BYTES;

    atomic_write_bytes(&target, after_bytes)?;
    guard.record_read(&target, after_bytes);

    let snapshot_id = record_file_snapshot(
        conn,
        chat_id,
        &target.to_string_lossy(),
        true,
        if truncated_or_unsaved { None } else { Some(before_bytes) },
        if truncated_or_unsaved { None } else { Some(after_bytes.to_vec()) },
        added,
        removed,
    )?;

    Ok(serde_json::json!({
        "path": target.to_string_lossy(),
        "snapshotId": snapshot_id,
        "addedLines": added,
        "removedLines": removed,
        "beforeHash": before_hash,
        "afterHash": after_hash,
        "truncatedOrUnsaved": truncated_or_unsaved,
    }))
}

pub fn write_file_atomic(
    conn: Option<&Connection>,
    chat_id: Option<&str>,
    path_str: &str,
    new_content: &str,
    overwrite: bool,
    allowed_roots: &[PathBuf],
    guard: &crate::tools::safety::FileReadGuard,
) -> Result<FileMutationResult, String> {
    let target = validate_path(path_str, allowed_roots)?;
    crate::tools::safety::SecretShield::check_path(&target)?;
    let existed_before = target.exists();
    if existed_before && !overwrite {
        return Err(format!("File already exists and overwrite=false: {}", path_str));
    }
    if existed_before {
        guard.verify_before_write(&target)?;
    }

    let before_blob = if existed_before { fs::read(&target).ok() } else { None };
    let old_text = before_blob
        .as_ref()
        .map(|b| String::from_utf8_lossy(b).to_string())
        .unwrap_or_default();
    let (added_lines, removed_lines) = compute_line_diff(&old_text, new_content);

    let after_bytes = new_content.as_bytes();
    let too_large = (after_bytes.len() as u64) > crate::tools::blob_store::MAX_BLOB_BYTES
        || before_blob.as_ref().map(|b| (b.len() as u64) > crate::tools::blob_store::MAX_BLOB_BYTES).unwrap_or(false);
    atomic_write_bytes(&target, after_bytes)?;
    guard.record_read(&target, after_bytes);

    let snapshot_id = record_file_snapshot(
        conn,
        chat_id,
        &target.to_string_lossy(),
        existed_before,
        if too_large { None } else { before_blob },
        if too_large { None } else { Some(after_bytes.to_vec()) },
        added_lines,
        removed_lines,
    )?;

    Ok(FileMutationResult {
        snapshot_id,
        path: target.to_string_lossy().to_string(),
        existed_before,
        added_lines,
        removed_lines,
        bytes_written: after_bytes.len(),
    })
}

pub fn fs_ops_safe(
    action: &str,
    path_str: &str,
    dest_str: Option<&str>,
    allowed_roots: &[PathBuf],
) -> Result<serde_json::Value, String> {
    let target = validate_path(path_str, allowed_roots)?;
    crate::tools::safety::SecretShield::check_path(&target)?;

    match action {
        "mkdir" => {
            fs::create_dir_all(&target).map_err(|e| e.to_string())?;
            Ok(serde_json::json!({ "action": "mkdir", "path": target.to_string_lossy() }))
        }
        "move" | "rename" => {
            let dest_raw = dest_str.ok_or("Missing dest for move/rename")?;
            let dest = validate_path(dest_raw, allowed_roots)?;
            crate::tools::safety::SecretShield::check_path(&dest)?;
            if let Some(p) = dest.parent() {
                fs::create_dir_all(p).map_err(|e| e.to_string())?;
            }
            fs::rename(&target, &dest).map_err(|e| e.to_string())?;
            Ok(serde_json::json!({ "action": "move", "from": target.to_string_lossy(), "to": dest.to_string_lossy() }))
        }
        "copy" => {
            let dest_raw = dest_str.ok_or("Missing dest for copy")?;
            let dest = validate_path(dest_raw, allowed_roots)?;
            crate::tools::safety::SecretShield::check_path(&dest)?;
            if let Some(p) = dest.parent() {
                fs::create_dir_all(p).map_err(|e| e.to_string())?;
            }
            fs::copy(&target, &dest).map_err(|e| e.to_string())?;
            Ok(serde_json::json!({ "action": "copy", "from": target.to_string_lossy(), "to": dest.to_string_lossy() }))
        }
        "delete" => {
            if !target.exists() {
                return Err(format!("Path does not exist: {}", path_str));
            }
            let _base = allowed_roots.first().cloned().unwrap_or_else(|| target.parent().unwrap_or(Path::new(".")).to_path_buf());
            let trash_dir = crate::state::AppState::resolve_chat_temp_dir(None).join("trash");
            fs::create_dir_all(&trash_dir).map_err(|e| e.to_string())?;
            let fname = target.file_name().and_then(|n| n.to_str()).unwrap_or("item");
            let trashed = trash_dir.join(format!("{}_{}", chrono::Utc::now().timestamp_millis(), fname));
            fs::rename(&target, &trashed).map_err(|e| e.to_string())?;
            Ok(serde_json::json!({ "action": "delete", "trashedTo": trashed.to_string_lossy() }))
        }
        other => Err(format!("Unsupported fs_ops action: {}", other)),
    }
}

pub fn glob_sorted_by_mtime(
    pattern: &str,
    root_opt: Option<&str>,
    limit: Option<usize>,
    allowed_roots: &[PathBuf],
) -> Result<Vec<String>, String> {
    let raw_root = match root_opt {
        Some(r) if !r.trim().is_empty() => validate_path(r, allowed_roots)?,
        _ => allowed_roots.first().cloned().unwrap_or_else(|| PathBuf::from(".")),
    };
    let root = raw_root.canonicalize().unwrap_or(raw_root);
    let root_norm = root
        .to_string_lossy()
        .replace('\\', "/")
        .trim_start_matches("//?/")
        .trim_end_matches('/')
        .to_lowercase();

    let mut pat_norm = pattern.replace('\\', "/");
    while pat_norm.starts_with("./") {
        pat_norm = pat_norm[2..].to_string();
    }
    pat_norm = pat_norm.trim_start_matches('/').to_string();

    let regex_str = format!(
        "^{}$",
        regex::escape(&pat_norm)
            .replace(r"\*\*/", "(?:.*/)?")
            .replace(r"\*\*", ".*")
            .replace(r"\*", "[^/]*")
            .replace(r"\?", ".")
    );
    let re = RegexBuilder::new(&regex_str).case_insensitive(true).build().map_err(|e| e.to_string())?;

    let mut matches: Vec<(u128, String)> = Vec::new();
    let walker = WalkBuilder::new(&root)
        .hidden(false)
        .git_ignore(true)
        .filter_entry(|e| {
            let name = e.file_name().to_string_lossy();
            name != ".git" && name != "node_modules" && name != "target" && name != ".omniharness"
        })
        .build();

    for entry in walker.flatten() {
        if entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            let full_norm = entry
                .path()
                .to_string_lossy()
                .replace('\\', "/");
            let full_clean = full_norm.trim_start_matches("//?/");
            let rel_str = if full_clean.to_lowercase().starts_with(&root_norm) {
                full_clean[root_norm.len()..].trim_start_matches('/').to_string()
            } else {
                entry
                    .path()
                    .strip_prefix(&root)
                    .unwrap_or(entry.path())
                    .to_string_lossy()
                    .replace('\\', "/")
            };
            if re.is_match(&rel_str) {
                let mtime = crate::tools::safety::FileReadGuard::file_mtime_nanos(entry.path());
                matches.push((mtime, full_clean.to_string()));
            }
        }
    }

    matches.sort_by(|a, b| b.0.cmp(&a.0));
    let cap = limit.unwrap_or(100);
    Ok(matches.into_iter().take(cap).map(|(_, p)| p).collect())
}

pub fn grep_with_context(
    pattern: &str,
    root_opt: Option<&str>,
    file_type: Option<&str>,
    context_lines: usize,
    allowed_roots: &[PathBuf],
) -> Result<Vec<serde_json::Value>, String> {
    let root = match root_opt {
        Some(r) if !r.trim().is_empty() => validate_path(r, allowed_roots)?,
        _ => allowed_roots.first().cloned().unwrap_or_else(|| PathBuf::from(".")),
    };
    enum LineMatcher {
        Simple(regex::Regex),
        Lookaround {
            main: regex::Regex,
            pos: Vec<regex::Regex>,
            neg: Vec<regex::Regex>,
        },
        Literal(String),
    }

    let matcher = match RegexBuilder::new(pattern).build() {
        Ok(r) => LineMatcher::Simple(r),
        Err(orig_err) => {
            if pattern.contains("(?!") || pattern.contains("(?=") {
                let mut pos_pats = Vec::new();
                let mut neg_pats = Vec::new();
                let mut cleaned = String::new();
                let bytes = pattern.as_bytes();
                let mut i = 0;
                while i < bytes.len() {
                    if i + 2 < bytes.len() && &bytes[i..i + 3] == b"(?!" || (i + 2 < bytes.len() && &bytes[i..i + 3] == b"(?=") {
                        let is_neg = bytes[i + 2] == b'!';
                        let mut depth = 1usize;
                        let start = i + 3;
                        let mut j = start;
                        while j < bytes.len() && depth > 0 {
                            if bytes[j] == b'\\' {
                                j += 2;
                                continue;
                            }
                            if bytes[j] == b'(' { depth += 1; }
                            else if bytes[j] == b')' { depth -= 1; }
                            j += 1;
                        }
                        if depth == 0 {
                            let inner = &pattern[start..j - 1];
                            if let Ok(r) = RegexBuilder::new(inner).build() {
                                if is_neg { neg_pats.push(r); } else { pos_pats.push(r); }
                            }
                            i = j;
                            continue;
                        }
                    }
                    let ch = pattern[i..].chars().next().unwrap();
                    cleaned.push(ch);
                    i += ch.len_utf8();
                }
                let main_pat = if cleaned.trim().is_empty() { ".*" } else { cleaned.as_str() };
                match RegexBuilder::new(main_pat).build() {
                    Ok(main_re) => LineMatcher::Lookaround { main: main_re, pos: pos_pats, neg: neg_pats },
                    Err(_) => return Err(orig_err.to_string()),
                }
            } else {
                LineMatcher::Literal(pattern.to_string())
            }
        }
    };
    let mut results = Vec::new();

    let walker = WalkBuilder::new(&root)
        .hidden(false)
        .git_ignore(true)
        .filter_entry(|e| {
            let name = e.file_name().to_string_lossy();
            name != ".git" && name != "node_modules" && name != "target" && name != ".omniharness"
        })
        .build();

    for entry in walker.flatten() {
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        if let Some(ft) = file_type {
            let ext = entry.path().extension().and_then(|e| e.to_str()).unwrap_or("");
            if !ext.eq_ignore_ascii_case(ft.trim_start_matches('.')) {
                continue;
            }
        }
        if let Ok(content) = fs::read_to_string(entry.path()) {
            let lines: Vec<&str> = content.lines().collect();
            for (idx, line) in lines.iter().enumerate() {
                let is_hit = match &matcher {
                    LineMatcher::Simple(r) => r.is_match(line),
                    LineMatcher::Lookaround { main, pos, neg } => {
                        main.is_match(line)
                            && pos.iter().all(|p| p.is_match(line))
                            && !neg.iter().any(|n| n.is_match(line))
                    }
                    LineMatcher::Literal(s) => line.contains(s.as_str()),
                };
                if is_hit {
                    let from = idx.saturating_sub(context_lines);
                    let to = (idx + context_lines + 1).min(lines.len());
                    let snippet: Vec<String> = (from..to)
                        .map(|i| format!("L{}: {}", i + 1, lines[i]))
                        .collect();
                    results.push(serde_json::json!({
                        "file": entry.path().to_string_lossy(),
                        "line": idx + 1,
                        "snippet": snippet.join("\n"),
                    }));
                    if results.len() >= 80 {
                        return Ok(results);
                    }
                }
            }
        }
    }

    Ok(results)
}
