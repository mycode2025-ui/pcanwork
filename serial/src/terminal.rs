use chrono::Local;
use encoding_rs::{CoderResult, Decoder, GB18030, GBK};
use std::collections::VecDeque;
use std::fs;
use std::path::Path;

pub struct TerminalState {
    buffer: TerminalBuffer,
    decoder: StreamDecoder,
    encoding: i32,
    ansi_enabled: bool,
    escape_state: EscapeState,
    style: AnsiStyle,
    history: CommandHistory,
}

impl TerminalState {
    pub fn new(encoding: i32, history_capacity: usize) -> Self {
        Self {
            buffer: TerminalBuffer::default(),
            decoder: StreamDecoder::new(encoding),
            encoding,
            ansi_enabled: true,
            escape_state: EscapeState::Ground,
            style: AnsiStyle::default(),
            history: CommandHistory::new(history_capacity),
        }
    }

    pub fn configure(
        &mut self,
        encoding: i32,
        ansi_enabled: bool,
        timestamp_mode: i32,
        max_lines_index: i32,
        tab_width: i32,
        history_capacity: i32,
    ) {
        if self.encoding != encoding {
            self.encoding = encoding;
            self.decoder = StreamDecoder::new(encoding);
        }
        self.ansi_enabled = ansi_enabled;
        self.buffer.timestamp_mode = timestamp_mode.clamp(0, 3);
        self.buffer.max_lines = max_lines(max_lines_index);
        self.buffer.tab_width = tab_width.clamp(1, 16) as usize;
        self.history.set_capacity(history_capacity.max(1) as usize);
    }

    pub fn push_bytes(&mut self, bytes: &[u8]) {
        let decoded = self.decoder.decode(bytes);
        for ch in decoded.chars() {
            self.push_char(ch);
        }
        self.buffer.cap_lines();
    }

    pub fn push_notice(&mut self, level: &str, message: &str) {
        self.buffer.begin_fresh_line();
        let line = format!(
            "[{}] [{}] {}",
            Local::now().format("%H:%M:%S%.3f"),
            level,
            message
        );
        self.buffer.write_plain(&line);
        self.buffer.newline();
        self.buffer.cap_lines();
    }

    pub fn push_local_echo(&mut self, command: &str) {
        self.buffer.begin_fresh_line();
        self.buffer.write_plain(&format!("TX > {command}"));
        self.buffer.newline();
        self.buffer.cap_lines();
    }

    pub fn clear(&mut self) {
        self.buffer.clear();
        self.escape_state = EscapeState::Ground;
        self.style = AnsiStyle::default();
    }

    pub fn text(&self) -> String {
        self.buffer.render()
    }

    pub fn add_history(&mut self, command: &str) {
        self.history.add(command);
    }

    pub fn navigate_history(&mut self, direction: i32, current: &str) -> String {
        self.history.navigate(direction, current)
    }

    pub fn load_history(&mut self, path: &Path) {
        let Ok(text) = fs::read_to_string(path) else {
            return;
        };
        self.history.load(text.lines());
    }

    pub fn save_history(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(
            path,
            self.history
                .entries
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join("\n"),
        )
    }

    fn push_char(&mut self, ch: char) {
        let state = std::mem::replace(&mut self.escape_state, EscapeState::Ground);
        match state {
            EscapeState::Ground => {
                if ch == '\u{1b}' {
                    self.escape_state = EscapeState::Escape;
                } else {
                    self.buffer.control_or_write(ch);
                }
            }
            EscapeState::Escape => match ch {
                '[' => self.escape_state = EscapeState::Csi(String::new()),
                ']' => self.escape_state = EscapeState::Osc { saw_escape: false },
                'c' if self.ansi_enabled => self.clear(),
                '7' | '8' | 'D' | 'E' | 'M' => {
                    // Recognized VT100 single-character commands. The line terminal
                    // intentionally ignores save/restore and reverse-index semantics.
                }
                _ => {}
            },
            EscapeState::Csi(mut sequence) => {
                if ('@'..='~').contains(&ch) {
                    if self.ansi_enabled {
                        self.execute_csi(&sequence, ch);
                    }
                } else if sequence.len() < 96 {
                    sequence.push(ch);
                    self.escape_state = EscapeState::Csi(sequence);
                }
            }
            EscapeState::Osc { mut saw_escape } => {
                if ch == '\u{7}' || (saw_escape && ch == '\\') {
                    return;
                }
                saw_escape = ch == '\u{1b}';
                self.escape_state = EscapeState::Osc { saw_escape };
            }
        }
    }

    fn execute_csi(&mut self, raw: &str, final_char: char) {
        let private_mode = raw.starts_with('?');
        let clean = raw.trim_start_matches(['?', '>', '!']);
        let params = clean
            .split(';')
            .map(|p| p.parse::<usize>().unwrap_or(0))
            .collect::<Vec<_>>();
        let first = params.first().copied().unwrap_or(0);
        match final_char {
            'm' => self.style.apply_sgr(&params),
            'K' => self.buffer.clear_line(first),
            'J' if first == 2 || first == 3 => {
                // 行式终端保留历史，把 ANSI “清屏”作为末尾的新屏幕帧。
                self.buffer.clear_screen_frame();
            }
            'J' => {}
            'A' => self.buffer.move_rows(-(first.max(1) as isize)),
            'B' => self.buffer.move_rows(first.max(1) as isize),
            'C' => self.buffer.move_columns(first.max(1) as isize),
            'D' => self.buffer.move_columns(-(first.max(1) as isize)),
            'G' => self.buffer.set_column(first.max(1) - 1),
            'H' | 'f' => {
                let row = params.first().copied().unwrap_or(1).max(1) - 1;
                let col = params.get(1).copied().unwrap_or(1).max(1) - 1;
                self.buffer.set_position(row, col);
            }
            'd' => self.buffer.set_row(first.max(1) - 1),
            'h' if private_mode && params.contains(&1049) => self.buffer.begin_screen_frame(),
            'l' if private_mode && params.contains(&1049) => self.buffer.finish_screen_frame(),
            _ => {}
        }
    }
}

#[derive(Default)]
struct TerminalBuffer {
    lines: Vec<Vec<char>>,
    prefixes: Vec<usize>,
    cursor_line: usize,
    cursor_col: usize,
    timestamp_mode: i32,
    max_lines: Option<usize>,
    tab_width: usize,
    screen_anchor: Option<usize>,
}

impl TerminalBuffer {
    fn ensure_line(&mut self) {
        if self.lines.is_empty() {
            self.lines.push(Vec::new());
            self.prefixes.push(0);
        }
        while self.cursor_line >= self.lines.len() {
            self.lines.push(Vec::new());
            self.prefixes.push(0);
        }
    }

    fn ensure_timestamp(&mut self) {
        self.ensure_line();
        if self.lines[self.cursor_line].is_empty() && self.timestamp_mode != 0 {
            let prefix = match self.timestamp_mode {
                1 => format!("[{}] ", Local::now().format("%H:%M:%S")),
                2 => format!("[{}] ", Local::now().format("%H:%M:%S%.3f")),
                _ => format!("[{}] ", Local::now().format("%Y-%m-%d %H:%M:%S%.3f")),
            };
            let chars = prefix.chars().collect::<Vec<_>>();
            self.prefixes[self.cursor_line] = chars.len();
            self.cursor_col = chars.len();
            self.lines[self.cursor_line] = chars;
        }
    }

    fn control_or_write(&mut self, ch: char) {
        match ch {
            '\r' => self.carriage_return(),
            '\n' => self.newline(),
            '\u{8}' | '\u{7f}' => self.backspace(),
            '\t' => self.tab(),
            '\0' => {}
            c if c.is_control() => {}
            c => self.write_char(c),
        }
    }

    fn write_char(&mut self, ch: char) {
        self.ensure_timestamp();
        let line = &mut self.lines[self.cursor_line];
        if self.cursor_col < line.len() {
            line[self.cursor_col] = ch;
        } else {
            line.resize(self.cursor_col, ' ');
            line.push(ch);
        }
        self.cursor_col += 1;
    }

    fn write_plain(&mut self, text: &str) {
        for ch in text.chars() {
            self.write_char(ch);
        }
    }

    fn newline(&mut self) {
        self.ensure_line();
        self.cursor_line += 1;
        if self.cursor_line >= self.lines.len() {
            self.lines.push(Vec::new());
            self.prefixes.push(0);
        }
        self.cursor_col = self.prefixes[self.cursor_line];
    }

    fn carriage_return(&mut self) {
        self.ensure_line();
        self.cursor_col = self.prefixes[self.cursor_line];
    }

    fn backspace(&mut self) {
        self.ensure_line();
        let prefix = self.prefixes[self.cursor_line];
        if self.cursor_col <= prefix {
            return;
        }
        self.cursor_col -= 1;
        let line = &mut self.lines[self.cursor_line];
        if self.cursor_col < line.len() {
            line.remove(self.cursor_col);
        }
    }

    fn tab(&mut self) {
        self.ensure_timestamp();
        let width = self.tab_width.max(1);
        let count = width - ((self.cursor_col - self.prefixes[self.cursor_line]) % width);
        for _ in 0..count {
            self.write_char(' ');
        }
    }

    fn begin_fresh_line(&mut self) {
        self.ensure_line();
        if !self.lines[self.cursor_line].is_empty() {
            self.newline();
        }
    }

    fn clear_line(&mut self, mode: usize) {
        self.ensure_line();
        let prefix = self.prefixes[self.cursor_line];
        let line = &mut self.lines[self.cursor_line];
        match mode {
            1 => {
                let end = self.cursor_col.min(line.len());
                for ch in &mut line[prefix.min(end)..end] {
                    *ch = ' ';
                }
            }
            2 => {
                line.clear();
                self.prefixes[self.cursor_line] = 0;
                self.cursor_col = 0;
            }
            _ => line.truncate(self.cursor_col.min(line.len())),
        }
    }

    fn move_rows(&mut self, delta: isize) {
        self.ensure_line();
        if delta < 0 && self.screen_anchor.is_none() {
            self.screen_anchor = Some(self.cursor_line);
        }
        let floor = self.screen_anchor.unwrap_or(0) as isize;
        let target = (self.cursor_line as isize + delta).max(floor) as usize;
        self.cursor_line = target;
        self.ensure_line();
        self.cursor_col = self.cursor_col.max(self.prefixes[self.cursor_line]);
    }

    fn move_columns(&mut self, delta: isize) {
        self.ensure_line();
        let prefix = self.prefixes[self.cursor_line] as isize;
        self.cursor_col = (self.cursor_col as isize + delta).max(prefix) as usize;
    }

    fn set_column(&mut self, column: usize) {
        self.ensure_line();
        self.cursor_col = self.prefixes[self.cursor_line] + column;
    }

    fn set_position(&mut self, row: usize, column: usize) {
        let anchor = self.ensure_screen_anchor();
        self.cursor_line = anchor + row;
        self.ensure_line();
        self.cursor_col = self.prefixes[self.cursor_line] + column;
    }

    fn set_row(&mut self, row: usize) {
        let anchor = self.ensure_screen_anchor();
        self.cursor_line = anchor + row;
        self.ensure_line();
        self.cursor_col = self.cursor_col.max(self.prefixes[self.cursor_line]);
    }

    fn ensure_screen_anchor(&mut self) -> usize {
        if let Some(anchor) = self.screen_anchor {
            return anchor;
        }
        self.ensure_line();
        if !self.lines[self.cursor_line].is_empty() {
            self.newline();
        }
        self.screen_anchor = Some(self.cursor_line);
        self.cursor_line
    }

    fn begin_screen_frame(&mut self) {
        let anchor = self.ensure_screen_anchor();
        self.lines.truncate(anchor + 1);
        self.prefixes.truncate(anchor + 1);
        self.lines[anchor].clear();
        self.prefixes[anchor] = 0;
        self.cursor_line = anchor;
        self.cursor_col = 0;
    }

    fn clear_screen_frame(&mut self) {
        self.begin_screen_frame();
    }

    fn finish_screen_frame(&mut self) {
        self.ensure_line();
        self.cursor_line = self.lines.len().saturating_sub(1);
        self.cursor_col = self.lines[self.cursor_line].len();
        if !self.lines[self.cursor_line].is_empty() {
            self.newline();
        }
        self.screen_anchor = None;
    }

    fn clear(&mut self) {
        self.lines.clear();
        self.prefixes.clear();
        self.cursor_line = 0;
        self.cursor_col = 0;
        self.screen_anchor = None;
        self.ensure_line();
    }

    fn cap_lines(&mut self) {
        let Some(limit) = self.max_lines else { return };
        if self.lines.len() <= limit {
            return;
        }
        let drop_count = self.lines.len() - limit;
        self.lines.drain(..drop_count);
        self.prefixes.drain(..drop_count);
        self.cursor_line = self.cursor_line.saturating_sub(drop_count);
        self.screen_anchor = self
            .screen_anchor
            .map(|anchor| anchor.saturating_sub(drop_count));
    }

    fn render(&self) -> String {
        self.lines
            .iter()
            .map(|line| line.iter().collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

enum StreamDecoder {
    Utf8(Vec<u8>),
    Legacy(Decoder),
    Latin1,
    Ascii,
}

impl StreamDecoder {
    fn new(encoding: i32) -> Self {
        match encoding {
            1 => Self::Legacy(GBK.new_decoder_with_bom_removal()),
            2 => Self::Legacy(GB18030.new_decoder_with_bom_removal()),
            3 => Self::Latin1,
            4 => Self::Ascii,
            _ => Self::Utf8(Vec::new()),
        }
    }

    fn decode(&mut self, bytes: &[u8]) -> String {
        match self {
            Self::Utf8(pending) => decode_utf8(pending, bytes),
            Self::Legacy(decoder) => {
                let mut output = String::with_capacity(bytes.len().saturating_mul(4).max(16));
                let mut offset = 0;
                loop {
                    let (result, read, _) =
                        decoder.decode_to_string(&bytes[offset..], &mut output, false);
                    offset += read;
                    if result == CoderResult::InputEmpty {
                        break;
                    }
                    output.reserve(bytes.len().saturating_mul(2).max(16));
                }
                output
            }
            Self::Latin1 => bytes.iter().map(|byte| char::from(*byte)).collect(),
            Self::Ascii => bytes
                .iter()
                .map(|byte| {
                    if byte.is_ascii() {
                        char::from(*byte)
                    } else {
                        '\u{fffd}'
                    }
                })
                .collect(),
        }
    }
}

fn decode_utf8(pending: &mut Vec<u8>, bytes: &[u8]) -> String {
    pending.extend_from_slice(bytes);
    let mut output = String::new();
    loop {
        match std::str::from_utf8(pending) {
            Ok(text) => {
                output.push_str(text);
                pending.clear();
                break;
            }
            Err(error) => {
                let valid = error.valid_up_to();
                if valid > 0 {
                    output.push_str(unsafe { std::str::from_utf8_unchecked(&pending[..valid]) });
                    pending.drain(..valid);
                }
                if let Some(length) = error.error_len() {
                    output.push('\u{fffd}');
                    pending.drain(..length.min(pending.len()));
                } else {
                    break;
                }
            }
        }
    }
    output
}

enum EscapeState {
    Ground,
    Escape,
    Csi(String),
    Osc { saw_escape: bool },
}

#[derive(Default)]
struct AnsiStyle {
    foreground: Option<u8>,
    background: Option<u8>,
    bold: bool,
    underline: bool,
}

impl AnsiStyle {
    fn apply_sgr(&mut self, params: &[usize]) {
        let params = if params.is_empty() { &[0][..] } else { params };
        for value in params {
            match value {
                0 => *self = Self::default(),
                1 => self.bold = true,
                4 => self.underline = true,
                22 => self.bold = false,
                24 => self.underline = false,
                30..=37 | 90..=97 => self.foreground = Some(*value as u8),
                39 => self.foreground = None,
                40..=47 | 100..=107 => self.background = Some(*value as u8),
                49 => self.background = None,
                _ => {}
            }
        }
    }
}

struct CommandHistory {
    entries: VecDeque<String>,
    capacity: usize,
    position: usize,
    draft: String,
}

impl CommandHistory {
    fn new(capacity: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            capacity: capacity.max(1),
            position: 0,
            draft: String::new(),
        }
    }

    fn set_capacity(&mut self, capacity: usize) {
        self.capacity = capacity.max(1);
        while self.entries.len() > self.capacity {
            self.entries.pop_front();
        }
        self.position = self.position.min(self.entries.len());
    }

    fn add(&mut self, command: &str) {
        if command.trim().is_empty() {
            return;
        }
        if self.entries.back().map(String::as_str) != Some(command) {
            self.entries.push_back(command.to_owned());
        }
        while self.entries.len() > self.capacity {
            self.entries.pop_front();
        }
        self.position = self.entries.len();
        self.draft.clear();
    }

    fn navigate(&mut self, direction: i32, current: &str) -> String {
        if self.entries.is_empty() {
            return current.to_owned();
        }
        if direction < 0 {
            if self.position == self.entries.len() {
                self.draft = current.to_owned();
            }
            self.position = self.position.saturating_sub(1);
            self.entries[self.position].clone()
        } else if self.position + 1 < self.entries.len() {
            self.position += 1;
            self.entries[self.position].clone()
        } else {
            self.position = self.entries.len();
            self.draft.clone()
        }
    }

    fn load<'a>(&mut self, lines: impl Iterator<Item = &'a str>) {
        self.entries.clear();
        for line in lines {
            self.add(line);
        }
        self.position = self.entries.len();
    }
}

fn max_lines(index: i32) -> Option<usize> {
    match index {
        0 => Some(10_000),
        1 => Some(50_000),
        3 => Some(500_000),
        4 => None,
        _ => Some(100_000),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_utf8_character_is_not_replaced() {
        let mut state = TerminalState::new(0, 10);
        state.push_bytes(&[0xE4, 0xB8]);
        assert_eq!(state.text(), "");
        state.push_bytes(&[0xAD, 0xE6, 0x96, 0x87]);
        assert_eq!(state.text(), "中文");
    }

    #[test]
    fn split_gbk_character_is_not_replaced() {
        let (encoded, _, _) = GBK.encode("中文");
        let bytes = encoded.as_ref();
        let mut state = TerminalState::new(1, 10);
        state.push_bytes(&bytes[..1]);
        assert_eq!(state.text(), "");
        state.push_bytes(&bytes[1..]);
        assert_eq!(state.text(), "中文");
    }

    #[test]
    fn packet_boundaries_do_not_insert_newlines() {
        let mut state = TerminalState::new(0, 10);
        state.push_bytes(b"roo");
        state.push_bytes(b"t@dev");
        state.push_bytes(b"ice:~#");
        assert_eq!(state.text(), "root@device:~#");
    }

    #[test]
    fn crlf_and_carriage_return_are_terminal_aware() {
        let mut state = TerminalState::new(0, 10);
        state.push_bytes(b"line1\r\nline2\r\n");
        assert_eq!(state.text(), "line1\nline2\n");
        state.clear();
        state.push_bytes(b"Progress 10%\rProgress 20%\rProgress 30%");
        assert_eq!(state.text(), "Progress 30%");
    }

    #[test]
    fn backspace_and_ansi_clear_line_work() {
        let mut state = TerminalState::new(0, 10);
        state.push_bytes(b"abc\x08d");
        assert_eq!(state.text(), "abd");
        state.push_bytes(b"\x1b[2Kready");
        assert_eq!(state.text(), "ready");
    }

    #[test]
    fn ansi_sequence_can_cross_packets() {
        let mut state = TerminalState::new(0, 10);
        state.push_bytes(b"old\x1b[");
        state.push_bytes(b"2Knew");
        assert_eq!(state.text(), "new");
    }

    #[test]
    fn full_screen_cursor_home_stays_at_history_tail() {
        let mut state = TerminalState::new(0, 10);
        state.push_bytes(b"shell output\r\n");
        state.push_bytes(b"\x1b[?1049h\x1b[H\x1b[2Ktop frame 1");
        assert_eq!(state.text(), "shell output\ntop frame 1");
        state.push_bytes(b"\x1b[H\x1b[2Jtop frame 2");
        assert_eq!(state.text(), "shell output\ntop frame 2");
    }

    #[test]
    fn history_preserves_draft_and_skips_adjacent_duplicates() {
        let mut history = CommandHistory::new(500);
        history.add("ls");
        history.add("ls");
        history.add("pwd");
        assert_eq!(history.navigate(-1, "draft"), "pwd");
        assert_eq!(history.navigate(-1, "pwd"), "ls");
        assert_eq!(history.navigate(1, "ls"), "pwd");
        assert_eq!(history.navigate(1, "pwd"), "draft");
    }
}
