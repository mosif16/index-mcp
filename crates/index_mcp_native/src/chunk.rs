use std::cmp::{max, min};

#[derive(Debug, Clone)]
pub struct ChunkFragment {
    pub content: String,
    pub byte_start: u32,
    pub byte_end: u32,
    pub line_start: u32,
    pub line_end: u32,
}

pub fn chunk_content(
    content: &str,
    chunk_size_tokens: usize,
    chunk_overlap_tokens: usize,
) -> Vec<ChunkFragment> {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }

    let chunk_char_limit = max(256usize, chunk_size_tokens.saturating_mul(4));
    let overlap_char_limit = chunk_overlap_tokens.saturating_mul(4);

    let mut char_byte_indices: Vec<usize> = Vec::new();
    let mut newline_char_indices: Vec<usize> = Vec::new();
    let mut line_start_char_indices: Vec<usize> = vec![0];

    let mut current_char_index = 0usize;
    for (byte_index, ch) in trimmed.char_indices() {
        char_byte_indices.push(byte_index);
        if ch == '\n' {
            newline_char_indices.push(current_char_index);
            line_start_char_indices.push(current_char_index + 1);
        }
        current_char_index += 1;
    }
    let total_chars = current_char_index;
    let total_bytes = trimmed.len();

    let mut fragments: Vec<ChunkFragment> = Vec::new();
    let mut start = 0usize;

    while start < total_chars {
        let mut end = min(total_chars, start.saturating_add(chunk_char_limit));

        if end < total_chars {
            let min_break = start + 200;
            if let Some(break_index) = find_break_index(&newline_char_indices, end, min_break) {
                end = break_index + 1;
            }
        }

        let start_byte = char_index_to_byte(start, &char_byte_indices, total_bytes);
        let mut end_byte = char_index_to_byte(end, &char_byte_indices, total_bytes);

        if end_byte < start_byte {
            end_byte = start_byte;
        }

        let raw_slice = &trimmed[start_byte..end_byte];
        let snippet = raw_slice.trim_end();

        if snippet.is_empty() {
            if end <= start {
                break;
            }
            start = end;
            continue;
        }

        let snippet_char_len = snippet.chars().count();
        let effective_end = start + snippet_char_len;
        let effective_end_byte = char_index_to_byte(effective_end, &char_byte_indices, total_bytes);

        let line_start = line_number_for_char(&line_start_char_indices, start);
        let line_end =
            line_number_for_char(&line_start_char_indices, effective_end.saturating_sub(1));

        fragments.push(ChunkFragment {
            content: snippet.to_string(),
            byte_start: start_byte as u32,
            byte_end: effective_end_byte as u32,
            line_start: line_start as u32,
            line_end: line_end as u32,
        });

        if effective_end >= total_chars {
            break;
        }

        let overlap_start = if overlap_char_limit > 0 {
            effective_end.saturating_sub(overlap_char_limit)
        } else {
            effective_end
        };

        if overlap_start > start {
            start = overlap_start;
        } else {
            start = effective_end;
        }
    }

    if fragments.is_empty() {
        return vec![fallback_fragment(trimmed)];
    }

    fragments
}

fn fallback_fragment(content: &str) -> ChunkFragment {
    let snippet = content.trim();
    if snippet.is_empty() {
        return ChunkFragment {
            content: String::new(),
            byte_start: 0,
            byte_end: 0,
            line_start: 1,
            line_end: 1,
        };
    }

    let byte_length = snippet.as_bytes().len() as u32;
    let line_count = snippet.lines().count().max(1) as u32;

    ChunkFragment {
        content: snippet.to_string(),
        byte_start: 0,
        byte_end: byte_length,
        line_start: 1,
        line_end: line_count,
    }
}

fn char_index_to_byte(index: usize, char_byte_indices: &[usize], total_bytes: usize) -> usize {
    if index == char_byte_indices.len() {
        total_bytes
    } else {
        char_byte_indices.get(index).copied().unwrap_or(total_bytes)
    }
}

fn find_break_index(newlines: &[usize], end: usize, min_break: usize) -> Option<usize> {
    if newlines.is_empty() {
        return None;
    }

    let mut lo = 0i64;
    let mut hi = (newlines.len() as i64) - 1;
    let mut candidate: Option<usize> = None;

    while lo <= hi {
        let mid = ((lo + hi) / 2) as usize;
        let value = newlines[mid];
        if value < end {
            candidate = Some(value);
            lo = mid as i64 + 1;
        } else {
            hi = mid as i64 - 1;
        }
    }

    if let Some(value) = candidate {
        if value >= min_break {
            return Some(value);
        }
    }

    None
}

fn line_number_for_char(line_starts: &[usize], target: usize) -> usize {
    if line_starts.is_empty() {
        return 1;
    }

    let mut lo = 0i64;
    let mut hi = (line_starts.len() as i64) - 1;
    let mut index = 0usize;

    while lo <= hi {
        let mid = ((lo + hi) / 2) as usize;
        let value = line_starts[mid];
        if value <= target {
            index = mid;
            lo = mid as i64 + 1;
        } else {
            hi = mid as i64 - 1;
        }
    }

    index + 1
}
