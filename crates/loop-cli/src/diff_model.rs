//! Pure data model for the `/diff` viewer: unified-diff parsing, per-hunk patch
//! building and hunk staleness fingerprinting. No I/O happens here.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

/// Hard cap on parsed files (see §3.2).
pub const MAX_FILES: usize = 2000;
/// Hard cap on total parsed diff body lines.
pub const MAX_TOTAL_LINES: usize = 250_000;

/// Which tree a diff was read against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffSide {
    /// Unstaged + untracked changes (`git diff`).
    WorkingTree,
    /// Index vs HEAD (`git diff --cached`).
    Index,
}

/// Status of a single file in the diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileStatus {
    Modified,
    Added,
    Deleted,
    Renamed,
    Copied,
    ModeOnly,
    Binary,
    /// Untracked file over the size/line cap; shown but not reviewable.
    TooLarge,
    /// File in a merge conflict (`diff --cc` / `* Unmerged path`); shown
    /// but not actionable until the conflict is resolved.
    Unmerged,
}

impl FileStatus {
    /// Single status letter shown in the viewer's file pane.
    pub fn letter(self) -> char {
        match self {
            FileStatus::Modified => 'M',
            FileStatus::Added => 'A',
            FileStatus::Deleted => 'D',
            FileStatus::Renamed => 'R',
            FileStatus::Copied => 'C',
            FileStatus::ModeOnly => 'm',
            FileStatus::Binary => 'B',
            FileStatus::TooLarge => 'L',
            FileStatus::Unmerged => 'U',
        }
    }
}

/// A single line inside a hunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: DiffLineKind,
    /// Line text without the leading marker character.
    pub text: String,
    /// True when followed by `\ No newline at end of file`.
    pub no_newline: bool,
    pub old_no: Option<u32>,
    pub new_no: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffLineKind {
    Context,
    Added,
    Removed,
}

/// Fingerprint of a hunk used to detect staleness after re-reading the repo.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct HunkId(pub u64);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    pub old_start: u32,
    pub old_count: u32,
    pub new_start: u32,
    pub new_count: u32,
    /// Text after the second `@@` in the header, if any.
    pub section: Option<String>,
    /// Ordered context/removed/added lines.
    pub lines: Vec<DiffLine>,
    /// Exact bytes of the hunk body from git, used to build patches.
    pub raw_body: Vec<u8>,
    pub id: HunkId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiff {
    /// `None` for newly created / untracked files.
    pub old_path: Option<String>,
    /// Repo-root-relative path used for display and actions.
    pub path: String,
    pub status: FileStatus,
    pub old_mode: Option<String>,
    pub new_mode: Option<String>,
    /// Synthesized all-added entry for an untracked file.
    pub untracked: bool,
    pub hunks: Vec<Hunk>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffModel {
    pub side: DiffSide,
    pub files: Vec<FileDiff>,
    /// Set when a cap (`MAX_FILES` / `MAX_TOTAL_LINES`) stopped parsing early.
    pub truncated: bool,
}

impl DiffModel {
    pub fn empty(side: DiffSide) -> Self {
        Self {
            side,
            files: Vec::new(),
            truncated: false,
        }
    }
}

/// Compute the staleness fingerprint for one hunk of a file: hashes the file
/// path, the hunk header numbers, every line kind + text and the raw body
/// bytes. Deliberately ignores the `index` line so the same logical change
/// keeps its id across re-reads.
pub fn hunk_id(file: &FileDiff, hunk: &Hunk) -> HunkId {
    let mut hasher = DefaultHasher::new();
    file.path.hash(&mut hasher);
    file.untracked.hash(&mut hasher);
    hunk.old_start.hash(&mut hasher);
    hunk.old_count.hash(&mut hasher);
    hunk.new_start.hash(&mut hasher);
    hunk.new_count.hash(&mut hasher);
    for line in &hunk.lines {
        discriminant(line.kind).hash(&mut hasher);
        line.text.hash(&mut hasher);
        line.no_newline.hash(&mut hasher);
    }
    hunk.raw_body.hash(&mut hasher);
    HunkId(hasher.finish())
}

fn discriminant(kind: DiffLineKind) -> u8 {
    match kind {
        DiffLineKind::Context => b' ',
        DiffLineKind::Added => b'+',
        DiffLineKind::Removed => b'-',
    }
}

/// Build a minimal `git apply`-able patch containing exactly one hunk of a
/// file, preserving the original hunk numbers. Returns `None` when the hunk
/// index is out of range or the file has no textual hunks. The patch is raw
/// bytes: patches may carry arbitrary (even non-UTF-8) file content.
pub fn build_single_hunk_patch(file: &FileDiff, hunk_idx: usize) -> Option<Vec<u8>> {
    let hunk = file.hunks.get(hunk_idx)?;
    let mut out = String::with_capacity(128 + hunk.lines.len() * 80);

    // `git apply` (default `-p1`) strips the first path component of every
    // name in the header, so names must carry the `a/` / `b/` prefixes (or be
    // `/dev/null`). When quoting is needed, the whole name — including the
    // prefix — goes inside the quotes, e.g. `"a/my file.txt"`. Git keeps
    // `a/… b/…` on both sides of `diff --git` even for adds/deletes and only
    // uses `/dev/null` in the `---`/`+++` lines.
    let a_disp = match &file.old_path {
        Some(p) => quote_header_name("a", p),
        None => quote_header_name("b", &file.path),
    };
    let b_disp = quote_header_name("b", &file.path);
    let old_disp = if file.old_path.is_some() {
        a_disp.clone()
    } else {
        "/dev/null".to_string()
    };
    let new_disp = if file.status == FileStatus::Deleted {
        "/dev/null".to_string()
    } else {
        b_disp.clone()
    };

    out.push_str("diff --git ");
    out.push_str(&a_disp);
    out.push(' ');
    out.push_str(&b_disp);
    out.push('\n');

    // Extended headers. A real `git diff` prints exactly one of these per
    // status; adds/deletes get `new file mode`/`deleted file mode`. Hunk
    // patches never carry a mode change: accepting or rejecting one hunk
    // must not also flip the file's permissions, so mode lines for tracked
    // files are dropped.
    match file.status {
        FileStatus::Added => {
            if let Some(m) = &file.new_mode {
                out.push_str("new file mode ");
                out.push_str(m);
                out.push('\n');
            }
        }
        FileStatus::Deleted => {
            if let Some(m) = &file.old_mode {
                out.push_str("deleted file mode ");
                out.push_str(m);
                out.push('\n');
            }
        }
        _ => {}
    }

    out.push_str("--- ");
    out.push_str(&old_disp);
    out.push('\n');
    out.push_str("+++ ");
    out.push_str(&new_disp);
    out.push('\n');

    write_hunk_header(&mut out, hunk);
    // The hunk body is git's exact bytes: `\r` line endings and non-UTF-8
    // content survive verbatim so the patch matches the file on disk.
    if hunk.raw_body.is_empty() {
        return None;
    }
    let mut patch = out.into_bytes();
    patch.extend_from_slice(&hunk.raw_body);
    Some(patch)
}

fn write_hunk_header(out: &mut String, hunk: &Hunk) {
    out.push_str("@@ -");
    push_range(out, hunk.old_start, hunk.old_count);
    out.push_str(" +");
    push_range(out, hunk.new_start, hunk.new_count);
    out.push_str(" @@");
    if let Some(section) = &hunk.section {
        out.push(' ');
        out.push_str(section);
    }
    out.push('\n');
}

fn push_range(out: &mut String, start: u32, count: u32) {
    if count == 1 {
        // `@@ -l +l @@` is the canonical single-line form.
        out.push_str(&start.to_string());
    } else if count == 0 {
        // Git prints `start,0` where start is the line *before* which the
        // (empty) range sits; we store the raw header numbers, so echo them.
        out.push_str(&start.to_string());
        out.push_str(",0");
    } else {
        out.push_str(&start.to_string());
        out.push(',');
        out.push_str(&count.to_string());
    }
}

/// Quote a path the way git does when it contains special characters.
fn quote_path(path: &str) -> String {
    if path.is_empty()
        || path
            .bytes()
            .any(|b| b <= 0x20 || b == 0x22 || b == 0x5c || b >= 0x7f)
    {
        let mut s = String::with_capacity(path.len() + 2);
        s.push('"');
        for c in path.chars() {
            match c {
                '"' => s.push_str("\\\""),
                '\\' => s.push_str("\\\\"),
                '\u{7}' => s.push_str("\\a"),
                '\u{8}' => s.push_str("\\b"),
                '\t' => s.push_str("\\t"),
                '\n' => s.push_str("\\n"),
                '\u{b}' => s.push_str("\\v"),
                '\u{c}' => s.push_str("\\f"),
                '\r' => s.push_str("\\r"),
                c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                    s.push_str(&format!("\\{:03o}", c as u32));
                }
                c => s.push(c),
            }
        }
        s.push('"');
        s
    } else {
        path.to_string()
    }
}

/// Quote a full diff header name — prefix included — the way git does:
/// `"a/my file.txt"`, never `a/"my file.txt"`.
fn quote_header_name(prefix: &str, path: &str) -> String {
    quote_path(&format!("{prefix}/{path}"))
}

/// Undo git's C-style quoting (used when `core.quotePath` mangles paths).
fn unquote_path(raw: &str) -> String {
    let bytes = raw.as_bytes();
    if bytes.len() < 2 || bytes[0] != b'"' || bytes[bytes.len() - 1] != b'"' {
        return raw.to_string();
    }
    let inner = &raw[1..raw.len() - 1];
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('a') => out.push('\u{7}'),
            Some('b') => out.push('\u{8}'),
            Some('t') => out.push('\t'),
            Some('n') => out.push('\n'),
            Some('v') => out.push('\u{b}'),
            Some('f') => out.push('\u{c}'),
            Some('r') => out.push('\r'),
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some(d) if ('0'..='7').contains(&d) => {
                let mut oct = d.to_digit(8).unwrap_or(0);
                let mut taken = 1;
                while taken < 3 {
                    match chars.clone().next() {
                        Some(n) if ('0'..='7').contains(&n) => {
                            oct = oct * 8 + n.to_digit(8).unwrap_or(0);
                            chars.next();
                            taken += 1;
                        }
                        _ => break,
                    }
                }
                if let Some(ch) = char::from_u32(oct) {
                    out.push(ch);
                }
            }
            Some(other) => {
                // Unknown escape: keep both characters verbatim.
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// Strip one trailing `\r` byte (Windows line endings, see R7).
fn strip_cr_bytes(line: &[u8]) -> &[u8] {
    line.strip_suffix(b"\r").unwrap_or(line)
}

/// Parse a full `git diff` output blob into a [`DiffModel`].
///
/// Total function: malformed input never panics; unexpected lines simply end
/// the current hunk/file entry and prior entries are kept. Parsing stops once
/// a cap is hit and sets `model.truncated`.
pub fn parse_unified_diff(raw: impl AsRef<[u8]>, side: DiffSide) -> DiffModel {
    let raw = raw.as_ref();
    let mut model = DiffModel::empty(side);
    let mut cur_file: Option<FileDiff> = None;
    let mut cur_hunk: Option<Hunk> = None;
    let mut total_lines: usize = 0;
    // Running line counters for the open hunk (see `next_line_numbers`) and
    // the number of old/new lines still expected from its `@@` header. Both
    // hitting 0 ends the hunk, so a body line like `--- comment` (a removed
    // SQL comment) is never mistaken for a file header.
    let mut old_no: u32 = 0;
    let mut new_no: u32 = 0;
    let mut old_left: u32 = 0;
    let mut new_left: u32 = 0;

    // Split on `\n`, dropping the trailing empty part when the input ends
    // with a newline. Each raw line keeps its exact bytes (a trailing `\r`
    // included) for the patch body; display text strips it and decodes
    // lossily.
    let mut lines: Vec<&[u8]> = raw.split(|&b| b == b'\n').collect();
    if raw.ends_with(b"\n") {
        lines.pop();
    }

    let finish_hunk = |cur_file: &mut Option<FileDiff>, cur_hunk: &mut Option<Hunk>| {
        if let Some(hunk) = cur_hunk.take() {
            if let Some(file) = cur_file.as_mut() {
                file.hunks.push(hunk);
            }
        }
    };

    for raw_line in lines {
        let line = String::from_utf8_lossy(strip_cr_bytes(raw_line)).into_owned();
        let line = line.as_str();
        // A hunk whose declared old/new line counts are both exhausted is
        // complete; it stays open only long enough to pick up a trailing
        // `\ No newline at end of file` marker.
        if cur_hunk.is_some() && old_left == 0 && new_left == 0 && !line.starts_with('\\') {
            finish_hunk(&mut cur_file, &mut cur_hunk);
        }

        if let Some(rest) = line.strip_prefix("diff --git ") {
            finish_hunk(&mut cur_file, &mut cur_hunk);
            if let Some(file) = cur_file.take() {
                push_file(&mut model, file);
            }
            if model.files.len() >= MAX_FILES {
                model.truncated = true;
                return model;
            }
            cur_file = Some(parse_git_header(rest));
            continue;
        }

        // Combined diffs (`diff --cc`, printed for conflicted files during a
        // merge) and the `* Unmerged path` marker (staged side of a conflict)
        // both describe a file the viewer can show but not act on. Only
        // recognized outside a hunk: inside one they are ordinary body text.
        let unmerged_name = if let Some(rest) = line
            .strip_prefix("diff --cc ")
            .or_else(|| line.strip_prefix("diff --combined "))
        {
            Some(rest.trim())
        } else {
            line.strip_prefix("* Unmerged path ").map(str::trim)
        };
        if let Some(name) = unmerged_name {
            finish_hunk(&mut cur_file, &mut cur_hunk);
            if let Some(file) = cur_file.take() {
                push_file(&mut model, file);
            }
            if model.files.len() >= MAX_FILES {
                model.truncated = true;
                return model;
            }
            let name = unquote_maybe(name);
            cur_file = Some(FileDiff {
                old_path: Some(name.clone()),
                path: name,
                status: FileStatus::Unmerged,
                old_mode: None,
                new_mode: None,
                untracked: false,
                hunks: Vec::new(),
            });
            continue;
        }

        // Extended headers that must land on the file *before* we borrow it.
        if line.starts_with("old mode ") || line.starts_with("new mode ") {
            finish_hunk(&mut cur_file, &mut cur_hunk);
            if let Some(file) = cur_file.as_mut() {
                if let Some(rest) = line.strip_prefix("old mode ") {
                    file.old_mode = Some(rest.trim().to_string());
                } else if let Some(rest) = line.strip_prefix("new mode ") {
                    file.new_mode = Some(rest.trim().to_string());
                }
            }
            continue;
        }

        let file = match cur_file.as_mut() {
            Some(f) => f,
            None => continue, // junk before any `diff --git`
        };

        // Extended headers.
        if line.starts_with("index ") || line.starts_with("similarity index ") {
            continue;
        } else if let Some(rest) = line.strip_prefix("deleted file mode ") {
            file.status = FileStatus::Deleted;
            // The mode given is the (only) pre-image mode.
            file.old_mode = Some(rest.trim().to_string());
            continue;
        } else if let Some(rest) = line.strip_prefix("new file mode ") {
            file.status = FileStatus::Added;
            // A newly created file has no old side at all.
            file.old_path = None;
            file.new_mode = Some(rest.trim().to_string());
            continue;
        } else if let Some(rest) = line.strip_prefix("copy from ") {
            file.status = FileStatus::Copied;
            file.old_path = Some(unquote_maybe(rest));
            continue;
        } else if let Some(rest) = line.strip_prefix("copy to ") {
            file.status = FileStatus::Copied;
            file.path = unquote_maybe(rest);
            continue;
        } else if let Some(rest) = line.strip_prefix("rename from ") {
            file.status = FileStatus::Renamed;
            file.old_path = Some(unquote_maybe(rest));
            continue;
        } else if let Some(rest) = line.strip_prefix("rename to ") {
            file.status = FileStatus::Renamed;
            file.path = unquote_maybe(rest);
            continue;
        } else if line.starts_with("Binary files ") || line.starts_with("GIT binary patch") {
            file.status = FileStatus::Binary;
            file.hunks.clear();
            cur_hunk = None;
            continue;
        } else if (line.starts_with("--- ") || line.starts_with("+++ ")) && cur_hunk.is_none() {
            // Path info is already known from the `diff --git` header; skip.
            // Only headers when no hunk is open: inside a hunk body these are
            // ordinary removed/added lines whose text happens to start with
            // `-- `/`++ ` (e.g. a SQL comment).
            continue;
        }

        if let Some((header, section)) = parse_hunk_header(line) {
            finish_hunk(&mut cur_file, &mut cur_hunk);
            old_no = header.0;
            new_no = header.2;
            old_left = header.1;
            new_left = header.3;
            cur_hunk = Some(Hunk {
                old_start: header.0,
                old_count: header.1,
                new_start: header.2,
                new_count: header.3,
                section,
                lines: Vec::new(),
                raw_body: Vec::new(),
                id: HunkId(0),
            });
            continue;
        }

        // Hunk body lines.
        if cur_hunk.is_some() {
            // "\ No newline at end of file" applies to the previous line.
            if line.starts_with('\\') {
                if let Some(hunk) = cur_hunk.as_mut() {
                    if let Some(last) = hunk.lines.last_mut() {
                        last.no_newline = true;
                    }
                    // The marker belongs to the patch body too.
                    hunk.raw_body.extend_from_slice(raw_line);
                    hunk.raw_body.push(b'\n');
                }
                continue;
            }
            let (kind, text) = if let Some(t) = line.strip_prefix('+') {
                (DiffLineKind::Added, t)
            } else if let Some(t) = line.strip_prefix('-') {
                (DiffLineKind::Removed, t)
            } else if let Some(t) = line.strip_prefix(' ') {
                (DiffLineKind::Context, t)
            } else if line.is_empty() {
                (DiffLineKind::Context, "")
            } else {
                // Garbage inside a hunk: end it gracefully, keep the entry.
                finish_hunk(&mut cur_file, &mut cur_hunk);
                continue;
            };

            total_lines += 1;
            if total_lines > MAX_TOTAL_LINES {
                model.truncated = true;
                finish_hunk(&mut cur_file, &mut cur_hunk);
                if let Some(f) = cur_file.take() {
                    push_file(&mut model, f);
                }
                return model;
            }

            if let Some(hunk) = cur_hunk.as_mut() {
                let (old_line, new_line) = next_line_numbers(kind, &mut old_no, &mut new_no);
                hunk.lines.push(DiffLine {
                    kind,
                    text: text.to_string(),
                    no_newline: false,
                    old_no: old_line,
                    new_no: new_line,
                });
                // Keep the exact bytes (marker, `\r`, non-UTF-8 content) so
                // a patch built from this hunk matches the file on disk.
                hunk.raw_body.extend_from_slice(raw_line);
                hunk.raw_body.push(b'\n');
            }
            match kind {
                DiffLineKind::Context => {
                    old_left = old_left.saturating_sub(1);
                    new_left = new_left.saturating_sub(1);
                }
                DiffLineKind::Removed => old_left = old_left.saturating_sub(1),
                DiffLineKind::Added => new_left = new_left.saturating_sub(1),
            }
        }
    }

    finish_hunk(&mut cur_file, &mut cur_hunk);
    if let Some(f) = cur_file.take() {
        push_file(&mut model, f);
    }
    model
}

fn push_file(model: &mut DiffModel, mut file: FileDiff) {
    if file.status == FileStatus::Modified
        && file.old_mode.is_some()
        && file.new_mode.is_some()
        && file.old_mode != file.new_mode
        && file.hunks.is_empty()
    {
        file.status = FileStatus::ModeOnly;
    }
    let ids: Vec<HunkId> = file.hunks.iter().map(|h| hunk_id(&file, h)).collect();
    for (hunk, id) in file.hunks.iter_mut().zip(ids) {
        hunk.id = id;
    }
    model.files.push(file);
}

/// Line numbers for the next hunk body line, given the hunk's start numbers
/// and running counters of old/new lines seen so far (O(1); the caller keeps
/// the counters across lines instead of re-counting the whole hunk).
fn next_line_numbers(
    kind: DiffLineKind,
    old_no: &mut u32,
    new_no: &mut u32,
) -> (Option<u32>, Option<u32>) {
    match kind {
        DiffLineKind::Context => {
            let n = (*old_no, *new_no);
            *old_no += 1;
            *new_no += 1;
            (Some(n.0), Some(n.1))
        }
        DiffLineKind::Removed => {
            let n = *old_no;
            *old_no += 1;
            (Some(n), None)
        }
        DiffLineKind::Added => {
            let n = *new_no;
            *new_no += 1;
            (None, Some(n))
        }
    }
}

/// Parse the two paths out of a `diff --git a/x b/x` header line.
fn parse_git_header(rest: &str) -> FileDiff {
    let (old_raw, new_raw) = match split_git_paths(rest) {
        Some(pair) => pair,
        None => (String::new(), rest.trim().to_string()),
    };
    let old_path = old_raw
        .strip_prefix("a/")
        .map(str::to_string)
        .unwrap_or_else(|| old_raw.clone());
    let new_path = new_raw
        .strip_prefix("b/")
        .map(str::to_string)
        .unwrap_or_else(|| new_raw.clone());
    FileDiff {
        old_path: Some(old_path),
        path: new_path,
        status: FileStatus::Modified,
        old_mode: None,
        new_mode: None,
        untracked: false,
        hunks: Vec::new(),
    }
}

/// Split the two names out of a `diff --git a/<path> b/<path>` header,
/// keeping their `a/` and `b/` prefixes; the caller strips them.
///
/// Tried in order of reliability:
///
/// * **Quoted first name** (`"a/my file.txt" "b/my file.txt"`): scan to its
///   closing quote, honoring `\` escapes. The second name is whatever
///   follows, minus leading spaces; both names go through [`unquote_path`],
///   so a quoted second name (tab, newline, …) is unescaped too.
/// * **Odd-length unquoted tail**: except for renames and copies the two
///   names are identical, so the single separating space sits exactly in
///   the middle of an odd-length tail. When the halves match after dropping
///   their (mandatory) `a/` and `b/` prefixes, that is the split. This is
///   what keeps names that themselves contain ` b/` (think
///   `plan b/notes.txt`) in one piece — searching for the first ` b/` would
///   cut such a name in half.
/// * **Fallback**: split at the first ` b/`. Only renames/copies with
///   differing names get here, and their `rename from` / `rename to` (or
///   `copy from` / `copy to`) lines carry the real names anyway.
fn split_git_paths(rest: &str) -> Option<(String, String)> {
    if rest.is_empty() {
        return None;
    }
    if rest.starts_with('"') {
        // Quoted first path: find its closing quote.
        let bytes = rest.as_bytes();
        let mut i = 1;
        while i < bytes.len() {
            if bytes[i] == b'\\' {
                i += 2;
                continue;
            }
            if bytes[i] == b'"' {
                let first = &rest[..=i];
                let second = rest[i + 1..].trim_start();
                return Some((unquote_path(first), unquote_path(second)));
            }
            i += 1;
        }
        return None;
    }
    // Identical names (every status but rename/copy) make the tail
    // odd-length with the separating space dead center. Try that midpoint
    // before trusting a ` b/` search, which mis-splits names containing
    // ` b/` themselves.
    if rest.len() % 2 == 1 {
        let mid = (rest.len() - 1) / 2;
        if rest.is_char_boundary(mid) && rest.as_bytes()[mid] == b' ' {
            let (left, right) = (&rest[..mid], &rest[mid + 1..]);
            if let (Some(l), Some(r)) = (left.strip_prefix("a/"), right.strip_prefix("b/")) {
                if l == r {
                    return Some((left.to_string(), right.to_string()));
                }
            }
        }
    }
    // Fallback: git separates the two paths with " b/". Renames and copies
    // land here; their `rename from`/`rename to` (or `copy from`/`copy to`)
    // lines then set the real names.
    let idx = rest.find(" b/")?;
    let first = rest[..idx].to_string();
    let second = rest[idx + 1..].to_string();
    Some((first, second))
}

fn unquote_maybe(raw: &str) -> String {
    if raw.starts_with('"') {
        unquote_path(raw)
    } else {
        raw.to_string()
    }
}

/// Parse `@@ -a,b +c,d @@ section`. Returns `(old_start, old_count,
/// new_start, new_count), section`.
/// Header numbers of a hunk: `(old_start, old_count, new_start, new_count)`.
type HunkHeader = (u32, u32, u32, u32);

fn parse_hunk_header(line: &str) -> Option<(HunkHeader, Option<String>)> {
    let rest = line.strip_prefix("@@ ")?;
    let end = rest.find(" @@")?;
    let body = &rest[..end];
    let tail = &rest[end + 3..];
    let section = if tail.is_empty() {
        None
    } else {
        // The section heading follows a single space after the closing `@@`.
        Some(tail.strip_prefix(' ').unwrap_or(tail).to_string())
    };

    let mut parts = body.split(' ');
    let old = parse_range(parts.next()?)?;
    let new = parse_range(parts.next()?)?;
    if parts.next().is_some() {
        return None;
    }
    Some(((old.0, old.1, new.0, new.1), section))
}

fn parse_range(s: &str) -> Option<(u32, u32)> {
    let s = s.strip_prefix('-').or_else(|| s.strip_prefix('+'))?;
    match s.split_once(',') {
        Some((start, count)) => Some((start.parse().ok()?, count.parse().ok()?)),
        None => Some((s.parse().ok()?, 1)),
    }
}

#[cfg(test)]
mod tests {

    /// Patch text for assertions: `build_single_hunk_patch` returns raw
    /// bytes, so tests that inspect the text convert lossily first.
    fn patch_text(patch: &[u8]) -> String {
        String::from_utf8_lossy(patch).into_owned()
    }

    use super::*;

    const TWO_FILE_DIFF: &str = "\
diff --git a/src/app.rs b/src/app.rs
index 0a1b2c3..4d5e6f7 100644
--- a/src/app.rs
+++ b/src/app.rs
@@ -41,7 +41,9 @@ fn main() {
 let x = 0;
 let y = 1;
-let z = 2;
+let z = 3;
+let w = 4;
 let a = 5;
 let b = 6;
 let c = 7;
 let d = 8;
+let v = 9;
diff --git a/README.md b/README.md
index 1111111..2222222 100644
--- a/README.md
+++ b/README.md
@@ -1,3 +1,2 @@
-old line
+new line
 tail
";
    const NO_NEWLINE_DIFF: &str = "\
diff --git a/x.txt b/x.txt
index 3333333..4444444 100644
--- a/x.txt
+++ b/x.txt
@@ -1 +1 @@
-a
+b
\\ No newline at end of file
";

    #[test]
    fn parses_two_file_diff_with_headers_and_lines() {
        let model = parse_unified_diff(TWO_FILE_DIFF, DiffSide::WorkingTree);
        assert!(!model.truncated);
        assert_eq!(model.side, DiffSide::WorkingTree);
        assert_eq!(model.files.len(), 2);

        let app = &model.files[0];
        assert_eq!(app.path, "src/app.rs");
        assert_eq!(app.old_path.as_deref(), Some("src/app.rs"));
        assert_eq!(app.status, FileStatus::Modified);
        assert_eq!(app.hunks.len(), 1);

        let hunk = &app.hunks[0];
        assert_eq!(hunk.old_start, 41);
        assert_eq!(hunk.old_count, 7);
        assert_eq!(hunk.new_start, 41);
        assert_eq!(hunk.new_count, 9);
        // Git keeps everything after the closing `@@` as the section,
        // including a trailing `{`.
        assert_eq!(hunk.section.as_deref(), Some("fn main() {"));

        // 5 context + 1 removed + 4 added = 10 lines, matching the
        // `-41,7 +41,9` header (old: 5+1, new: 5+4).
        assert_eq!(hunk.lines.len(), 10);
        assert_eq!(hunk.lines[0].kind, DiffLineKind::Context);
        assert_eq!(hunk.lines[0].old_no, Some(41));
        assert_eq!(hunk.lines[0].new_no, Some(41));
        assert_eq!(hunk.lines[2].kind, DiffLineKind::Removed);
        assert_eq!(hunk.lines[2].text, "let z = 2;");
        assert_eq!(hunk.lines[2].old_no, Some(43));
        assert_eq!(hunk.lines[2].new_no, None);
        assert_eq!(hunk.lines[3].kind, DiffLineKind::Added);
        assert_eq!(hunk.lines[3].text, "let z = 3;");
        assert_eq!(hunk.lines[3].old_no, None);
        assert_eq!(hunk.lines[3].new_no, Some(43));
        assert_eq!(hunk.lines[4].kind, DiffLineKind::Added);
        assert_eq!(hunk.lines[4].new_no, Some(44));

        let readme = &model.files[1];
        assert_eq!(readme.path, "README.md");
        assert_eq!(readme.hunks[0].old_start, 1);
        assert_eq!(readme.hunks[0].old_count, 3);
        assert_eq!(readme.hunks[0].new_count, 2);
        assert_eq!(readme.hunks[0].section, None);
    }

    #[test]
    fn parses_no_newline_marker() {
        let model = parse_unified_diff(NO_NEWLINE_DIFF, DiffSide::Index);
        let line = &model.files[0].hunks[0].lines[1];
        assert_eq!(line.kind, DiffLineKind::Added);
        assert!(line.no_newline);
        assert!(!model.files[0].hunks[0].lines[0].no_newline);
    }

    #[test]
    fn parses_rename_without_hunks() {
        let raw = "\
diff --git a/old_name.rs b/new_name.rs
similarity index 95%
rename from old_name.rs
rename to new_name.rs
index abcdef0..1234567 100644
";
        let model = parse_unified_diff(raw, DiffSide::WorkingTree);
        assert_eq!(model.files.len(), 1);
        let f = &model.files[0];
        assert_eq!(f.status, FileStatus::Renamed);
        assert_eq!(f.old_path.as_deref(), Some("old_name.rs"));
        assert_eq!(f.path, "new_name.rs");
        assert!(f.hunks.is_empty());
    }

    #[test]
    fn parses_mode_only_change() {
        let raw = "\
diff --git a/script.sh b/script.sh
old mode 100644
new mode 100755
";
        let model = parse_unified_diff(raw, DiffSide::WorkingTree);
        assert_eq!(model.files.len(), 1);
        let f = &model.files[0];
        assert_eq!(f.status, FileStatus::ModeOnly);
        assert_eq!(f.old_mode.as_deref(), Some("100644"));
        assert_eq!(f.new_mode.as_deref(), Some("100755"));
        assert!(f.hunks.is_empty());
    }

    #[test]
    fn parses_binary_file() {
        let raw = "\
diff --git a/logo.png b/logo.png
index 7777777..8888888 100644
Binary files a/logo.png and b/logo.png differ
";
        let model = parse_unified_diff(raw, DiffSide::WorkingTree);
        assert_eq!(model.files.len(), 1);
        assert_eq!(model.files[0].status, FileStatus::Binary);
        assert!(model.files[0].hunks.is_empty());
    }

    #[test]
    fn parses_deleted_and_created_files() {
        let raw = "\
diff --git a/gone.txt b/gone.txt
deleted file mode 100644
index 9999999..0000000
--- a/gone.txt
+++ /dev/null
@@ -1 +0 @@
-content
diff --git a/fresh.txt b/fresh.txt
new file mode 100644
index 0000000..abcdef1
--- /dev/null
+++ b/fresh.txt
@@ -0,0 +1 @@
+content
";
        let model = parse_unified_diff(raw, DiffSide::WorkingTree);
        assert_eq!(model.files.len(), 2);

        let gone = &model.files[0];
        assert_eq!(gone.status, FileStatus::Deleted);
        assert_eq!(gone.old_path.as_deref(), Some("gone.txt"));
        assert_eq!(gone.hunks.len(), 1);
        assert_eq!(gone.hunks[0].lines[0].kind, DiffLineKind::Removed);

        let fresh = &model.files[1];
        assert_eq!(fresh.status, FileStatus::Added);
        assert_eq!(fresh.old_path, None);
        assert_eq!(fresh.hunks.len(), 1);
        assert_eq!(fresh.hunks[0].lines[0].kind, DiffLineKind::Added);
    }

    #[test]
    fn parses_removed_dash_dash_comment_inside_hunk() {
        // Removing a SQL comment yields a body line `--- comment`; it must be
        // parsed as a removed line, not swallowed as a file header.
        let raw = "\
diff --git a/query.sql b/query.sql
--- a/query.sql
+++ b/query.sql
@@ -1,3 +1,2 @@
 SELECT 1;
--- comment
 SELECT 2;
";
        let model = parse_unified_diff(raw, DiffSide::WorkingTree);
        assert_eq!(model.files.len(), 1);
        let hunk = &model.files[0].hunks[0];
        assert_eq!(hunk.lines.len(), 3);
        assert_eq!(hunk.lines[1].kind, DiffLineKind::Removed);
        assert_eq!(hunk.lines[1].text, "-- comment");
        assert_eq!(hunk.lines[1].old_no, Some(2));
        assert_eq!(hunk.lines[1].new_no, None);
        assert_eq!(hunk.lines[2].kind, DiffLineKind::Context);
        assert_eq!(hunk.lines[2].old_no, Some(3));
        assert_eq!(hunk.lines[2].new_no, Some(2));
    }

    #[test]
    fn parses_removed_triple_dash_line_inside_hunk() {
        // A removed line whose text is `- x` prints as `--- x` — still a body
        // line while the hunk's declared counts are not exhausted.
        let raw = "\
diff --git a/doc.md b/doc.md
--- a/doc.md
+++ b/doc.md
@@ -1,3 +1,2 @@
 intro
--- x
 outro
";
        let model = parse_unified_diff(raw, DiffSide::WorkingTree);
        assert_eq!(model.files.len(), 1);
        let hunk = &model.files[0].hunks[0];
        assert_eq!(hunk.lines.len(), 3);
        assert_eq!(hunk.lines[1].kind, DiffLineKind::Removed);
        assert_eq!(hunk.lines[1].text, "-- x");
        // The next file's real `---`/`+++` headers still work afterwards.
        let raw_with_next = format!(
            "{raw}\
diff --git a/other.rs b/other.rs
--- a/other.rs
+++ b/other.rs
@@ -1 +1 @@
-a
+b
"
        );
        let model = parse_unified_diff(&raw_with_next, DiffSide::WorkingTree);
        assert_eq!(model.files.len(), 2);
        assert_eq!(model.files[1].path, "other.rs");
        assert_eq!(model.files[1].hunks.len(), 1);
        assert_eq!(model.files[1].hunks[0].lines.len(), 2);
    }

    #[test]
    fn parses_added_plus_plus_line_inside_hunk() {
        // Symmetric case for `+`: an added line whose text starts with `+ `
        // prints as `+++ …`.
        let raw = "\
diff --git a/note.md b/note.md
--- a/note.md
+++ b/note.md
@@ -1,2 +1,3 @@
 keep
+++ added emphasis
 keep2
";
        let model = parse_unified_diff(raw, DiffSide::WorkingTree);
        let hunk = &model.files[0].hunks[0];
        assert_eq!(hunk.lines.len(), 3);
        assert_eq!(hunk.lines[1].kind, DiffLineKind::Added);
        assert_eq!(hunk.lines[1].text, "++ added emphasis");
    }

    #[test]
    fn garbage_tail_keeps_prior_entries() {
        let raw =
            format!("{TWO_FILE_DIFF}complete garbage that is not diff syntax\n\x00binary junk\n");
        let model = parse_unified_diff(&raw, DiffSide::WorkingTree);
        assert!(!model.truncated);
        assert_eq!(model.files.len(), 2);
        assert_eq!(model.files[0].path, "src/app.rs");
        assert_eq!(model.files[1].hunks[0].lines.len(), 3);
    }

    #[test]
    fn empty_input_yields_empty_model() {
        let model = parse_unified_diff("", DiffSide::WorkingTree);
        assert!(model.files.is_empty());
        assert!(!model.truncated);
    }

    #[test]
    fn single_hunk_patch_reparses_identically() {
        let model = parse_unified_diff(TWO_FILE_DIFF, DiffSide::WorkingTree);
        let app = &model.files[0];
        let patch = patch_text(&build_single_hunk_patch(app, 0).expect("patch builds"));
        assert!(patch.starts_with("diff --git a/src/app.rs b/src/app.rs\n"));
        assert!(patch.contains("--- a/src/app.rs\n"));
        assert!(patch.contains("+++ b/src/app.rs\n"));

        let reparsed = parse_unified_diff(&patch, DiffSide::WorkingTree);
        assert_eq!(reparsed.files.len(), 1);
        assert_eq!(
            reparsed.files[0].hunks.len(),
            1,
            "exactly one hunk: {patch}"
        );
        let orig = &app.hunks[0];
        let again = &reparsed.files[0].hunks[0];
        assert_eq!(again.old_start, orig.old_start);
        assert_eq!(again.old_count, orig.old_count);
        assert_eq!(again.new_start, orig.new_start);
        assert_eq!(again.new_count, orig.new_count);
        assert_eq!(again.section, orig.section);
        assert_eq!(again.lines, orig.lines);
    }

    #[test]
    fn single_hunk_patch_carries_git_a_b_prefixes() {
        // `git apply` with default `-p1` strips one path component, so every
        // name must keep its `a/`/`b/` prefix. Root-level, nested and
        // space-containing paths all come from the same code path.
        let raw = "\
diff --git a/main.rs b/main.rs
--- a/main.rs
+++ b/main.rs
@@ -1,3 +1,3 @@
 fn main() {}
-old
+new
 tail
";
        let model = parse_unified_diff(raw, DiffSide::WorkingTree);
        let patch = patch_text(&build_single_hunk_patch(&model.files[0], 0).expect("patch builds"));
        assert!(patch.starts_with("diff --git a/main.rs b/main.rs\n"));
        assert!(patch.contains("--- a/main.rs\n"));
        assert!(patch.contains("+++ b/main.rs\n"));

        let raw = "\
diff --git a/src/app.rs b/src/app.rs
--- a/src/app.rs
+++ b/src/app.rs
@@ -1 +1 @@
-a
+b
";
        let model = parse_unified_diff(raw, DiffSide::WorkingTree);
        let patch = patch_text(&build_single_hunk_patch(&model.files[0], 0).expect("patch builds"));
        assert!(patch.starts_with("diff --git a/src/app.rs b/src/app.rs\n"));
        assert!(patch.contains("--- a/src/app.rs\n"));
        assert!(patch.contains("+++ b/src/app.rs\n"));

        // A quoted name includes the prefix inside the quotes.
        let raw = "\
diff --git \"a/my file.txt\" \"b/my file.txt\"
--- \"a/my file.txt\"
+++ \"b/my file.txt\"
@@ -1 +1 @@
-a
+b
";
        let model = parse_unified_diff(raw, DiffSide::WorkingTree);
        let patch = patch_text(&build_single_hunk_patch(&model.files[0], 0).expect("patch builds"));
        assert!(patch.starts_with("diff --git \"a/my file.txt\" \"b/my file.txt\"\n"));
        assert!(patch.contains("--- \"a/my file.txt\"\n"));
        assert!(patch.contains("+++ \"b/my file.txt\"\n"));
    }

    #[test]
    fn single_hunk_patch_out_of_range_is_none() {
        let model = parse_unified_diff(TWO_FILE_DIFF, DiffSide::WorkingTree);
        assert!(build_single_hunk_patch(&model.files[0], 1).is_none());
    }

    #[test]
    fn single_hunk_patch_preserves_no_newline() {
        let model = parse_unified_diff(NO_NEWLINE_DIFF, DiffSide::Index);
        let patch = patch_text(&build_single_hunk_patch(&model.files[0], 0).expect("patch builds"));
        assert!(patch.contains("\\ No newline at end of file\n"));
        let reparsed = parse_unified_diff(&patch, DiffSide::Index);
        let lines = &reparsed.files[0].hunks[0].lines;
        assert_eq!(lines.len(), 2);
        assert!(lines[1].no_newline);
    }

    #[test]
    fn single_hunk_patch_uses_dev_null_for_add_and_delete() {
        let raw = "\
diff --git a/gone.txt b/gone.txt
deleted file mode 100644
index 9999999..0000000
--- a/gone.txt
+++ /dev/null
@@ -1 +0 @@
-content
diff --git a/fresh.txt b/fresh.txt
new file mode 100644
index 0000000..abcdef1
--- /dev/null
+++ b/fresh.txt
@@ -0,0 +1 @@
+content
";
        let model = parse_unified_diff(raw, DiffSide::WorkingTree);
        let deleted =
            patch_text(&build_single_hunk_patch(&model.files[0], 0).expect("patch builds"));
        assert!(deleted.contains("--- a/gone.txt\n"));
        assert!(deleted.contains("+++ /dev/null\n"));
        let added = patch_text(&build_single_hunk_patch(&model.files[1], 0).expect("patch builds"));
        assert!(added.contains("--- /dev/null\n"));
        assert!(added.contains("+++ b/fresh.txt\n"));
    }

    #[test]
    fn single_hunk_patch_emits_new_and_deleted_file_mode_headers() {
        // Added files carry `new file mode <mode>`, deleted files
        // `deleted file mode <mode>` — exactly what git prints — never
        // `old mode`/`new mode`: hunk patches never carry a mode change, so
        // accepting or rejecting one hunk leaves permissions alone.
        let raw = "\
diff --git a/gone.txt b/gone.txt
deleted file mode 100644
index 9999999..0000000
--- a/gone.txt
+++ /dev/null
@@ -1 +0 @@
-content
diff --git a/fresh.txt b/fresh.txt
new file mode 100644
index 0000000..abcdef1
--- /dev/null
+++ b/fresh.txt
@@ -0,0 +1 @@
+content
";
        let model = parse_unified_diff(raw, DiffSide::WorkingTree);

        let deleted =
            patch_text(&build_single_hunk_patch(&model.files[0], 0).expect("patch builds"));
        assert!(
            deleted.contains("deleted file mode 100644\n"),
            "deleted patch: {deleted}"
        );
        assert!(!deleted.contains("old mode"));
        assert!(!deleted.contains("new mode"));

        let added = patch_text(&build_single_hunk_patch(&model.files[1], 0).expect("patch builds"));
        assert!(
            added.contains("new file mode 100644\n"),
            "added patch: {added}"
        );
        assert!(!added.contains("old mode"));
        assert!(!added.contains("deleted file mode"));
    }

    #[test]
    fn single_hunk_patch_drops_tracked_file_mode_change_headers() {
        // A modified tracked file whose mode also changed must not leak
        // `old mode`/`new mode` into the hunk patch: applying it (forward or
        // reversed) would silently flip the file's permissions.
        let raw = "\
diff --git a/s.sh b/s.sh
old mode 100644
new mode 100755
index 1111111..2222222
--- a/s.sh
+++ b/s.sh
@@ -1 +1 @@
-one
+two
";
        let model = parse_unified_diff(raw, DiffSide::WorkingTree);
        let patch = patch_text(&build_single_hunk_patch(&model.files[0], 0).expect("patch builds"));
        assert!(!patch.contains("old mode"), "{patch}");
        assert!(!patch.contains("new mode"), "{patch}");
        assert!(patch.contains("--- a/s.sh\n"), "{patch}");
        assert!(patch.contains("+++ b/s.sh\n"), "{patch}");
    }

    #[test]
    fn cr_only_change_gets_a_new_hunk_id() {
        // The same logical change with `\n` vs `\r\n` body line endings
        // displays identically but must hash differently: the raw bytes are
        // part of the staleness fingerprint.
        let lf = b"diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -1,3 +1,3 @@\n one\n-two\n+TWO\n three\n";
        let crlf = b"diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -1,3 +1,3 @@\n one\r\n-two\r\n+TWO\r\n three\r\n";
        let lf_model = parse_unified_diff(lf, DiffSide::WorkingTree);
        let crlf_model = parse_unified_diff(crlf, DiffSide::WorkingTree);
        assert_eq!(lf_model.files.len(), 1);
        assert_eq!(crlf_model.files.len(), 1);

        let lf_hunk = &lf_model.files[0].hunks[0];
        let crlf_hunk = &crlf_model.files[0].hunks[0];
        let lf_texts: Vec<&str> = lf_hunk.lines.iter().map(|l| l.text.as_str()).collect();
        let crlf_texts: Vec<&str> = crlf_hunk.lines.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(lf_texts, crlf_texts);
        assert_ne!(lf_hunk.id, crlf_hunk.id);
    }

    #[test]
    fn hunk_id_ignores_index_line_but_not_content() {
        let mk = |index_line: &str, body: &str| {
            let raw =
                format!("diff --git a/a.rs b/a.rs\n{index_line}\n--- a/a.rs\n+++ b/a.rs\n{body}");
            parse_unified_diff(&raw, DiffSide::WorkingTree)
                .files
                .remove(0)
        };
        let body = "@@ -1,3 +1,3 @@\n a\n-b\n+c\n";
        let a = mk("index 1111111..2222222 100644", body);
        let b = mk("index 9999999..8888888 100644", body);
        assert_eq!(a.hunks[0].id, b.hunks[0].id, "same change, different index");

        let edited = mk(
            "index 1111111..2222222 100644",
            "@@ -1,3 +1,3 @@\n a\n-b\n+CHANGED\n",
        );
        assert_ne!(a.hunks[0].id, edited.hunks[0].id, "edited line changes id");
    }

    #[test]
    fn hunk_id_depends_on_path() {
        let mk = |path: &str| {
            let raw = format!(
                "diff --git a/{p} b/{p}\n--- a/{p}\n+++ b/{p}\n@@ -1 +1 @@\n-a\n+b\n",
                p = path
            );
            let mut file = parse_unified_diff(&raw, DiffSide::WorkingTree)
                .files
                .remove(0);
            file.hunks[0].id = hunk_id(&file, &file.hunks[0]);
            file
        };
        let a = mk("one.rs");
        let b = mk("two.rs");
        assert_ne!(a.hunks[0].id, b.hunks[0].id);
    }

    #[test]
    fn caps_set_truncated_on_too_many_lines() {
        let mut raw = String::from(
            "diff --git a/big.txt b/big.txt\n--- a/big.txt\n+++ b/big.txt\n@@ -1,10 +1,10 @@\n",
        );
        for i in 0..MAX_TOTAL_LINES + 5 {
            raw.push_str(&format!("+line {i}\n"));
        }
        let model = parse_unified_diff(&raw, DiffSide::WorkingTree);
        assert!(model.truncated);
        assert_eq!(model.files.len(), 1);
        let total: usize = model.files[0].hunks.iter().map(|h| h.lines.len()).sum();
        assert_eq!(total, MAX_TOTAL_LINES, "cap is exact");
    }

    #[test]
    fn caps_set_truncated_on_too_many_files() {
        let mut raw = String::new();
        for i in 0..MAX_FILES + 5 {
            raw.push_str(&format!("diff --git a/f{i}.txt b/f{i}.txt\n"));
        }
        let model = parse_unified_diff(&raw, DiffSide::WorkingTree);
        assert!(model.truncated);
        assert_eq!(model.files.len(), MAX_FILES);
    }

    #[test]
    fn crlf_lines_are_handled() {
        let raw =
            "diff --git a/a.rs b/a.rs\r\n--- a/a.rs\r\n+++ b/a.rs\r\n@@ -1 +1 @@\r\n-a\r\n+b\r\n";
        let model = parse_unified_diff(raw, DiffSide::WorkingTree);
        assert_eq!(model.files.len(), 1);
        let hunk = &model.files[0].hunks[0];
        assert_eq!(hunk.lines.len(), 2);
        assert_eq!(hunk.lines[0].text, "a");
        assert_eq!(hunk.lines[1].text, "b");
    }

    #[test]
    fn quoted_paths_are_unquoted() {
        let raw = "diff --git \"a/with space.txt\" \"b/with space.txt\"\nindex 1111111..2222222 100644\n--- \"a/with space.txt\"\n+++ \"b/with space.txt\"\n@@ -1 +1 @@\n-a\n+b\n";
        let model = parse_unified_diff(raw, DiffSide::WorkingTree);
        assert_eq!(model.files.len(), 1);
        assert_eq!(model.files[0].path, "with space.txt");
        assert_eq!(model.files[0].old_path.as_deref(), Some("with space.txt"));
        assert_eq!(model.files[0].hunks.len(), 1);
    }

    #[test]
    fn parses_paths_containing_space_b_slash() {
        // A name that itself contains " b/" must not be cut at the first
        // " b/": with identical names the separating space sits exactly at
        // the midpoint of the odd-length tail.
        let raw = "\
diff --git a/plan b/notes.txt b/plan b/notes.txt
--- a/plan b/notes.txt
+++ b/plan b/notes.txt
@@ -1 +1 @@
-a
+b
";
        let model = parse_unified_diff(raw, DiffSide::WorkingTree);
        assert_eq!(model.files.len(), 1);
        assert_eq!(model.files[0].path, "plan b/notes.txt");
        assert_eq!(model.files[0].old_path.as_deref(), Some("plan b/notes.txt"));
        assert_eq!(model.files[0].hunks.len(), 1);

        let raw = "\
diff --git a/q b/x b/y.txt b/q b/x b/y.txt
--- a/q b/x b/y.txt
+++ b/q b/x b/y.txt
@@ -1 +1 @@
-a
+b
";
        let model = parse_unified_diff(raw, DiffSide::WorkingTree);
        assert_eq!(model.files.len(), 1);
        assert_eq!(model.files[0].path, "q b/x b/y.txt");
        assert_eq!(model.files[0].old_path.as_deref(), Some("q b/x b/y.txt"));

        // A rename's two names differ, so the midpoint check fails and the
        // header falls back to the " b/" split; the `rename from`/`rename
        // to` lines then carry the real names.
        let raw = "\
diff --git a/old b/a.txt b/new b/a.txt
similarity index 95%
rename from old b/a.txt
rename to new b/a.txt
";
        let model = parse_unified_diff(raw, DiffSide::WorkingTree);
        assert_eq!(model.files.len(), 1);
        assert_eq!(model.files[0].status, FileStatus::Renamed);
        assert_eq!(model.files[0].old_path.as_deref(), Some("old b/a.txt"));
        assert_eq!(model.files[0].path, "new b/a.txt");
    }

    #[test]
    fn parses_quoted_paths_on_both_sides() {
        // A tab forces git to quote both names; the second one must be
        // unquoted too, not only the first.
        let raw = concat!(
            "diff --git \"a/a\\tb.txt\" \"b/a\\tb.txt\"\n",
            "--- \"a/a\\tb.txt\"\n",
            "+++ \"b/a\\tb.txt\"\n",
            "@@ -1 +1 @@\n",
            "-a\n",
            "+b\n",
        );
        let model = parse_unified_diff(raw, DiffSide::WorkingTree);
        assert_eq!(model.files.len(), 1);
        assert_eq!(model.files[0].path, "a\tb.txt");
        assert_eq!(model.files[0].old_path.as_deref(), Some("a\tb.txt"));
        assert_eq!(model.files[0].hunks.len(), 1);
    }

    #[test]
    fn status_letters_are_distinct() {
        let letters = [
            FileStatus::Modified,
            FileStatus::Added,
            FileStatus::Deleted,
            FileStatus::Renamed,
            FileStatus::Copied,
            FileStatus::ModeOnly,
            FileStatus::Binary,
            FileStatus::TooLarge,
            FileStatus::Unmerged,
        ]
        .map(FileStatus::letter);
        let unique: std::collections::HashSet<_> = letters.iter().collect();
        assert_eq!(unique.len(), letters.len(), "{letters:?}");
    }

    #[test]
    fn parses_combined_diff_as_unmerged() {
        // What git prints for a conflicted file on the working-tree side
        // during a merge: a combined `diff --cc` header followed by `@@@`
        // hunk headers and prefixed body lines. The viewer lists the file
        // as Unmerged with no hunks; none of the body may leak into a hunk.
        let raw = "\
diff --cc f.txt
index b19a1e9,950b81b..0000000
--- a/f.txt
+++ b/f.txt
@@@ -1,1 -1,1 +1,5 @@@
++<<<<<<< HEAD
 +ours
++=======
+ theirs
++>>>>>>> other
diff --git a/other.txt b/other.txt
index 1111111..2222222 100644
--- a/other.txt
+++ b/other.txt
@@ -1 +1 @@
-a
+b
";
        let model = parse_unified_diff(raw, DiffSide::WorkingTree);
        assert_eq!(model.files.len(), 2);

        let f = &model.files[0];
        assert_eq!(f.path, "f.txt");
        assert_eq!(f.old_path.as_deref(), Some("f.txt"));
        assert_eq!(f.status, FileStatus::Unmerged);
        assert!(f.hunks.is_empty());

        // The next, ordinary file still parses normally.
        let other = &model.files[1];
        assert_eq!(other.path, "other.txt");
        assert_eq!(other.status, FileStatus::Modified);
        assert_eq!(other.hunks.len(), 1);
        assert_eq!(other.hunks[0].lines.len(), 2);
    }

    #[test]
    fn parses_unmerged_path_line() {
        // What git prints for a conflicted file on the staged side:
        // `* Unmerged path <path>`.
        let raw = "* Unmerged path f.txt\n";
        let model = parse_unified_diff(raw, DiffSide::Index);
        assert_eq!(model.files.len(), 1);
        let f = &model.files[0];
        assert_eq!(f.path, "f.txt");
        assert_eq!(f.old_path.as_deref(), Some("f.txt"));
        assert_eq!(f.status, FileStatus::Unmerged);
        assert!(f.hunks.is_empty());
    }

    #[test]
    fn untracked_flag_round_trips_through_id() {
        let mut file = FileDiff {
            old_path: None,
            path: "new.txt".to_string(),
            status: FileStatus::Added,
            old_mode: None,
            new_mode: None,
            untracked: true,
            hunks: vec![Hunk {
                old_start: 0,
                old_count: 0,
                new_start: 1,
                new_count: 1,
                section: None,
                lines: vec![DiffLine {
                    kind: DiffLineKind::Added,
                    text: "hello".to_string(),
                    no_newline: false,
                    old_no: None,
                    new_no: Some(1),
                }],
                raw_body: Vec::new(),
                id: HunkId(0),
            }],
        };
        file.hunks[0].id = hunk_id(&file, &file.hunks[0]);
        let tracked_id = file.hunks[0].id;
        file.untracked = false;
        assert_ne!(hunk_id(&file, &file.hunks[0]), tracked_id);
    }
}
