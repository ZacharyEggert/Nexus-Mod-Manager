//! OBMM Mod Script installers (`fomod/script`, `fomod/script.txt`; C# `Script Types/ModScript`,
//! plus the Oblivion/Morrowind extras), with a hand-written parser in place of the ANTLR grammar.
//!
//! Running a script doesn't touch the game: it builds a [`Plan`] (files to install, generated
//! files, INI edits, plugin changes) that `Manager::install` applies. UI calls ([`Prompt`]) are
//! answered by running the script again with the answers so far; it stops at the first one
//! still unanswered. Reads of game state (data files, INIs, plugins) see the plan's earlier steps.

use crate::fomod::{parse_version, Version};
use crate::ini::decode;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

/// What a script sees of the mod and the game.
pub struct Host {
    /// The mod's files relative to `base` ('/'-separated), `fomod*` left out (C# `FOMod.GetFileList`).
    pub files: Vec<String>,
    /// Folder holding `fomod/`; mod paths are relative to it.
    pub base: PathBuf,
    /// Where data files go (`Data`).
    pub install_root: PathBuf,
    pub plugin_exts: Vec<String>,
    /// Load order: (name, active).
    pub plugins: Vec<(String, bool)>,
    pub game: Option<Version>,
    pub extender: Option<Version>,
    pub game_ini: Option<PathBuf>,
    /// `RendererInfo.txt`.
    pub renderer: Option<PathBuf>,
    /// Oblivion/Morrowind: their extra functions (`EditINI`, `RegisterBSA`, …).
    pub gamebryo: bool,
}

/// One file the plan installs.
#[derive(Debug, Clone, PartialEq)]
pub enum Src {
    /// Path in the mod (actual case).
    Mod(String),
    /// Written by the script (`SetPluginByte`, `SetTextLine`, …).
    Bytes(Vec<u8>),
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Plan {
    /// Lowercase destination -> (destination, source); later writes win.
    pub files: BTreeMap<String, (String, Src)>,
    /// (file, section, key, value), in order.
    pub ini: Vec<(PathBuf, String, String, String)>,
    /// Plugins to leave inactive (`UncheckEsp`).
    pub uncheck: Vec<String>,
    /// Relative load orders to apply, in order (C# `SetRelativeLoadOrder`).
    pub orders: Vec<Vec<String>>,
}

impl Plan {
    fn put(&mut self, dest: String, src: Src) {
        self.files.insert(dest.to_lowercase(), (dest, src));
    }
    fn get(&self, dest: &str) -> Option<&Src> {
        self.files.get(&dest.to_lowercase()).map(|f| &f.1)
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SelectOption {
    pub name: String,
    pub description: Option<String>,
    /// Image path in the mod.
    pub image: Option<String>,
    pub default: bool,
}

/// A question for the user.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Prompt {
    /// Answer: `Pick` (exactly one index unless `many`).
    Select {
        title: String,
        many: bool,
        options: Vec<SelectOption>,
    },
    /// Answer: `Bool`.
    YesNo { title: String, message: String },
    /// Answer: anything.
    Message {
        title: Option<String>,
        message: String,
    },
    /// Answer: `Text`, or `Ack` for cancel.
    Input {
        title: Option<String>,
        initial: Option<String>,
    },
    /// A text file from the mod. Answer: anything.
    Text { title: Option<String>, text: String },
    /// An image from the mod (path in the mod). Answer: anything.
    Image { title: Option<String>, path: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum Answer {
    Bool(bool),
    Pick(Vec<usize>),
    Text(String),
    Ack(()),
}

#[derive(Debug)]
pub enum Outcome {
    Ask(Prompt),
    Done(Plan),
}

/// Run `code` with `answers` to its prompts so far. Errors are script errors (invalid script,
/// unknown function, `FatalError`) to show the user.
/// Shallowest `fomod/script` (preferred, as in C#) or `fomod/script.txt` file under `root`.
pub fn locate(root: &Path) -> Option<PathBuf> {
    ["script", "script.txt"].iter().find_map(|name| {
        walkdir::WalkDir::new(root)
            .sort_by_key(|e| e.depth())
            .into_iter()
            .filter_map(Result::ok)
            .find(|e| {
                e.file_type().is_file()
                    && e.file_name().eq_ignore_ascii_case(name)
                    && e.path()
                        .parent()
                        .and_then(|p| p.file_name())
                        .is_some_and(|n| n.eq_ignore_ascii_case("fomod"))
            })
            .map(|e| e.into_path())
    })
}

pub fn run(code: &str, host: &Host, answers: &[Answer]) -> Result<Outcome, String> {
    let prog =
        parse(&preprocess(&run_on_lines(code))).map_err(|e| format!("Invalid Mod Script: {e}"))?;
    let mut it = Interp::new(host, answers);
    let r = (|| {
        it.block(&prog)?;
        let mut jumps = 0;
        while it.goto {
            jumps += 1;
            if jumps > 100_000 {
                return Err(Halt::Fail("the script keeps jumping (Goto loop)".into()));
            }
            it.goto = false;
            it.find_label = true;
            it.block(&prog)?;
        }
        if it.fatal {
            return Err(Halt::Fail(
                "The script stopped the install (FatalError).".into(),
            ));
        }
        it.complete()
    })();
    match r {
        Ok(()) => Ok(Outcome::Done(it.plan)),
        Err(Halt::Ask(p)) => Ok(Outcome::Ask(p)),
        Err(Halt::Fail(e)) => Err(e),
    }
}

// ---------- lexing ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kw {
    Goto,
    Label,
    FatalError,
    IfNot,
    If,
    Else,
    EndIf,
    For,
    Continue,
    Exit,
    EndFor,
    Count,
    Each,
    ExecLines,
    SelectWithDescriptionsAndPreviews,
    SelectWithDescriptions,
    SelectWithPreview,
    SelectManyWithDescriptionsAndPreviews,
    SelectManyWithDescriptions,
    SelectManyWithPreview,
    SelectMany,
    SelectVar,
    SelectString,
    Select,
    Case,
    Default,
    Break,
    EndSelect,
    Return,
}

const KEYWORDS: &[(&str, Kw)] = &[
    ("goto", Kw::Goto),
    ("label", Kw::Label),
    ("fatalerror", Kw::FatalError),
    ("ifnot", Kw::IfNot),
    ("if", Kw::If),
    ("else", Kw::Else),
    ("endif", Kw::EndIf),
    ("for", Kw::For),
    ("continue", Kw::Continue),
    ("exit", Kw::Exit),
    ("endfor", Kw::EndFor),
    ("count", Kw::Count),
    ("each", Kw::Each),
    ("execlines", Kw::ExecLines),
    (
        "selectwithdescriptionsandpreviews",
        Kw::SelectWithDescriptionsAndPreviews,
    ),
    ("selectwithdescriptions", Kw::SelectWithDescriptions),
    ("selectwithpreview", Kw::SelectWithPreview),
    (
        "selectmanywithdescriptionsandpreviews",
        Kw::SelectManyWithDescriptionsAndPreviews,
    ),
    ("selectmanywithdescriptions", Kw::SelectManyWithDescriptions),
    ("selectmanywithpreview", Kw::SelectManyWithPreview),
    ("selectmany", Kw::SelectMany),
    ("selectvar", Kw::SelectVar),
    ("selectstring", Kw::SelectString),
    ("select", Kw::Select),
    ("case", Kw::Case),
    ("default", Kw::Default),
    ("break", Kw::Break),
    ("endselect", Kw::EndSelect),
    ("return", Kw::Return),
];

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Newline,
    Comma,
    Kw(Kw, String),
    /// Letters only (C# `FUNC_NAME`).
    Func(String),
    /// Any other argument: quoted, identifier, `%variable%`, bare string.
    Word(String),
}

struct Token {
    tok: Tok,
    line: usize,
}

/// C# `AllowRunOnLines`: drop the marker line, join lines ending in `\`.
fn run_on_lines(code: &str) -> String {
    if !code.lines().any(|l| l.trim() == "AllowRunOnLines") {
        return code.into();
    }
    let kept: Vec<char> = code
        .split_inclusive('\n')
        .filter(|l| l.trim() != "AllowRunOnLines")
        .flat_map(str::chars)
        .collect();
    let mut out = String::new();
    let mut i = 0;
    while i < kept.len() {
        if kept[i] == '\\' {
            // `\\\s*\n`, greedy: through the last newline of the whitespace run.
            let (mut j, mut nl) = (i + 1, None);
            while j < kept.len() && kept[j].is_whitespace() {
                if kept[j] == '\n' {
                    nl = Some(j);
                }
                j += 1;
            }
            if let Some(k) = nl {
                i = k + 1;
                continue;
            }
        }
        out.push(kept[i]);
        i += 1;
    }
    out
}

/// C# `GenerateAst` clean-up, two passes as there: outside quoted strings, unescape `\"` and
/// `\\`; then (strings found again) strip `;` comments.
fn preprocess(code: &str) -> String {
    let nl = |ch: char| ch == '\n' || ch == '\r';
    let unescaped = outside_strings(code, |c, i, out| match (c[*i], c.get(*i + 1)) {
        ('\\', Some(&n)) if n == '"' || n == '\\' => {
            out.push(n);
            *i += 2;
        }
        (ch, _) => {
            out.push(ch);
            *i += 1;
        }
    });
    outside_strings(&unescaped, |c, i, out| {
        if c[*i] == ';' {
            while *i < c.len() && !nl(c[*i]) {
                *i += 1;
            }
        } else {
            out.push(c[*i]);
            *i += 1;
        }
    })
}

/// Copy `code`, quoted strings verbatim and everything else through `step`.
fn outside_strings(code: &str, step: impl Fn(&[char], &mut usize, &mut String)) -> String {
    let c: Vec<char> = code.chars().collect();
    let mut out = String::with_capacity(code.len());
    let mut i = 0;
    while i < c.len() {
        if c[i] == '"' && (i == 0 || c[i - 1] != '\\') {
            let mut j = i + 1;
            while j < c.len() && !matches!(c[j], '\n' | '\r') && !(c[j] == '"' && c[j - 1] != '\\')
            {
                j += 1;
            }
            if j < c.len() && c[j] == '"' {
                out.extend(&c[i..=j]);
                i = j + 1;
                continue;
            }
        }
        step(&c, &mut i, &mut out);
    }
    out
}

fn lex(code: &str) -> Vec<Token> {
    let c: Vec<char> = code.chars().collect();
    let stop = |ch: char| matches!(ch, ' ' | '\t' | '\u{c}' | '\r' | '\n' | ',');
    let ident = |ch: char| ch.is_ascii_alphanumeric() || "_-|".contains(ch);
    let run =
        |from: usize, ok: &dyn Fn(char) -> bool| c[from..].iter().take_while(|&&ch| ok(ch)).count();
    let (mut toks, mut i, mut line) = (vec![], 0, 1);
    while i < c.len() {
        let tok = match c[i] {
            '\r' | '\n' => {
                i += if c[i] == '\r' && c.get(i + 1) == Some(&'\n') {
                    2
                } else {
                    1
                };
                toks.push(Token {
                    tok: Tok::Newline,
                    line,
                });
                line += 1;
                continue;
            }
            ' ' | '\t' | '\u{c}' => {
                i += 1;
                continue;
            }
            ',' => {
                i += 1;
                Tok::Comma
            }
            _ => {
                // Longest match wins, ties to the earlier ANTLR rule: keyword, QUOTED_LITERAL,
                // FUNC_NAME, IDENTIFIER, VARIABLE, STRING_LITERAL.
                let s = run(i, &|ch| !stop(ch));
                let a = run(i, &|ch| ch.is_ascii_alphabetic());
                let id = run(i, &ident);
                let v = match c[i] {
                    '%' => {
                        let k = run(i + 1, &ident);
                        if k > 0 && c.get(i + 1 + k) == Some(&'%') {
                            k + 2
                        } else {
                            0
                        }
                    }
                    _ => 0,
                };
                let q = if c[i] == '"' { quoted_len(&c, i) } else { 0 };
                let best = s.max(a).max(id).max(v).max(q);
                let text: String = c[i..i + best].iter().collect();
                i += best;
                if a == best {
                    match KEYWORDS.iter().find(|k| k.0.eq_ignore_ascii_case(&text)) {
                        Some(k) => Tok::Kw(k.1, text),
                        None => Tok::Func(text),
                    }
                } else {
                    Tok::Word(text)
                }
            }
        };
        toks.push(Token { tok, line });
    }
    toks
}

/// Length of the `"…"` at `i` (`\"` escapes a quote), 0 when unclosed on its line.
fn quoted_len(c: &[char], i: usize) -> usize {
    let close = |esc: bool| {
        let mut j = i + 1;
        while j < c.len() && c[j] != '\n' && c[j] != '\r' {
            if esc && c[j] == '\\' && c.get(j + 1) == Some(&'"') {
                j += 2;
                continue;
            }
            if c[j] == '"' {
                return j + 1 - i;
            }
            j += 1;
        }
        0
    };
    match close(true) {
        0 => close(false),
        n => n,
    }
}

// ---------- parsing ----------

#[derive(Debug, Clone)]
struct Func {
    name: String,
    args: Vec<String>,
    line: usize,
}

#[derive(Debug, Clone)]
struct Case {
    /// None = `Default`.
    matches: Option<Vec<String>>,
    body: Vec<Stmt>,
}

#[derive(Debug, Clone)]
enum Stmt {
    Call(Func),
    Goto(String),
    Label(String),
    ExecLines(String),
    Break(usize),
    Exit(usize),
    Continue(usize),
    Return,
    Fatal,
    If {
        not: bool,
        cond: Func,
        then: Vec<Stmt>,
        els: Vec<Stmt>,
    },
    Select {
        kind: Kw,
        title: String,
        /// Each option: name, then its description and/or image.
        options: Vec<Vec<String>>,
        cases: Vec<Case>,
    },
    SelectStr {
        var: bool,
        expr: String,
        cases: Vec<Case>,
    },
    ForCount {
        var: String,
        from: String,
        to: String,
        step: Option<String>,
        body: Vec<Stmt>,
    },
    ForEach {
        var: String,
        func: Func,
        body: Vec<Stmt>,
    },
}

struct Parser {
    toks: Vec<Token>,
    pos: usize,
}

fn parse(code: &str) -> Result<Vec<Stmt>, String> {
    let mut p = Parser {
        toks: lex(code),
        pos: 0,
    };
    let prog = p.block()?;
    p.newlines();
    if p.pos < p.toks.len() {
        return Err(p.unexpected());
    }
    if prog.is_empty() {
        return Err("no statements".into());
    }
    Ok(prog)
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos).map(|t| &t.tok)
    }
    fn line(&self) -> usize {
        self.toks
            .get(self.pos)
            .or(self.toks.last())
            .map_or(1, |t| t.line)
    }
    fn unexpected(&self) -> String {
        match self.peek() {
            None => format!("line {}: unexpected end of script", self.line()),
            Some(Tok::Newline) => format!("line {}: unexpected end of line", self.line()),
            Some(Tok::Comma) => format!("line {}: unexpected ','", self.line()),
            Some(Tok::Kw(_, t) | Tok::Func(t) | Tok::Word(t)) => {
                format!("line {}: unexpected '{t}'", self.line())
            }
        }
    }
    fn newlines(&mut self) {
        while self.peek() == Some(&Tok::Newline) {
            self.pos += 1;
        }
    }
    fn kw(&mut self, want: Kw) -> Result<(), String> {
        self.newlines();
        match self.peek() {
            Some(Tok::Kw(k, _)) if *k == want => {
                self.pos += 1;
                Ok(())
            }
            _ => Err(self.unexpected()),
        }
    }

    fn starts_stmt(&self) -> bool {
        use Kw::*;
        match self.peek() {
            Some(Tok::Func(_)) => true,
            Some(Tok::Kw(k, _)) => !matches!(
                k,
                Else | EndIf | EndFor | Count | Each | Case | Default | EndSelect
            ),
            _ => false,
        }
    }

    /// Statements up to the next `Else`/`EndIf`/`Case`/… or the end.
    fn block(&mut self) -> Result<Vec<Stmt>, String> {
        let mut out = vec![];
        loop {
            self.newlines();
            if !self.starts_stmt() {
                return Ok(out);
            }
            out.push(self.stmt()?);
            if !matches!(self.peek(), None | Some(Tok::Newline)) {
                return Err(self.unexpected());
            }
        }
    }

    /// One argument, with its optional trailing comma.
    fn arg(&mut self) -> Option<String> {
        let t = match self.peek()? {
            Tok::Kw(_, t) | Tok::Func(t) | Tok::Word(t) => t.clone(),
            _ => return None,
        };
        self.pos += 1;
        if self.peek() == Some(&Tok::Comma) {
            self.pos += 1;
        }
        Some(t)
    }
    fn need_arg(&mut self) -> Result<String, String> {
        self.arg().ok_or_else(|| self.unexpected())
    }
    fn args(&mut self) -> Vec<String> {
        std::iter::from_fn(|| self.arg()).collect()
    }

    fn func(&mut self) -> Result<Func, String> {
        let line = self.line();
        match self.peek() {
            Some(Tok::Func(name)) => {
                let name = name.clone();
                self.pos += 1;
                Ok(Func {
                    name,
                    args: self.args(),
                    line,
                })
            }
            _ => Err(self.unexpected()),
        }
    }

    fn cases(&mut self) -> Result<Vec<Case>, String> {
        let mut cases = vec![];
        loop {
            self.newlines();
            let matches = match self.peek() {
                Some(Tok::Kw(Kw::Case, _)) => {
                    self.pos += 1;
                    let m = self.args();
                    if m.is_empty() {
                        return Err(self.unexpected());
                    }
                    Some(m)
                }
                Some(Tok::Kw(Kw::Default, _)) => {
                    self.pos += 1;
                    None
                }
                Some(Tok::Kw(Kw::EndSelect, _)) if !cases.is_empty() => {
                    self.pos += 1;
                    return Ok(cases);
                }
                _ => return Err(self.unexpected()),
            };
            if !matches!(self.peek(), None | Some(Tok::Newline)) {
                return Err(self.unexpected());
            }
            cases.push(Case {
                matches,
                body: self.block()?,
            });
        }
    }

    fn stmt(&mut self) -> Result<Stmt, String> {
        use Kw::*;
        let line = self.line();
        let Some(Tok::Kw(k, _)) = self.peek().cloned() else {
            return Ok(Stmt::Call(self.func()?));
        };
        self.pos += 1;
        Ok(match k {
            Break => Stmt::Break(line),
            Exit => Stmt::Exit(line),
            Continue => Stmt::Continue(line),
            Return => Stmt::Return,
            FatalError => Stmt::Fatal,
            Goto => Stmt::Goto(self.need_arg()?),
            Label => Stmt::Label(self.need_arg()?),
            ExecLines => Stmt::ExecLines(self.need_arg()?),
            If | IfNot => {
                let cond = self.func()?;
                let then = self.block()?;
                self.newlines();
                let els = match self.peek() {
                    Some(Tok::Kw(Else, _)) => {
                        self.pos += 1;
                        self.block()?
                    }
                    _ => vec![],
                };
                self.kw(EndIf)?;
                Stmt::If {
                    not: k == IfNot,
                    cond,
                    then,
                    els,
                }
            }
            SelectVar | SelectString => {
                let expr = self.need_arg()?;
                Stmt::SelectStr {
                    var: k == SelectVar,
                    expr,
                    cases: self.cases()?,
                }
            }
            Select
            | SelectMany
            | SelectWithPreview
            | SelectManyWithPreview
            | SelectWithDescriptions
            | SelectManyWithDescriptions
            | SelectWithDescriptionsAndPreviews
            | SelectManyWithDescriptionsAndPreviews => {
                let title = self.need_arg()?;
                let per = match k {
                    Select | SelectMany => 1,
                    SelectWithDescriptionsAndPreviews | SelectManyWithDescriptionsAndPreviews => 3,
                    _ => 2,
                };
                let opts = self.args();
                if opts.is_empty() || !opts.len().is_multiple_of(per) {
                    return Err(format!(
                        "line {line}: each option needs {per} value(s), got {} for {}",
                        opts.len(),
                        opts.len().div_ceil(per)
                    ));
                }
                Stmt::Select {
                    kind: k,
                    title,
                    options: opts.chunks(per).map(<[String]>::to_vec).collect(),
                    cases: self.cases()?,
                }
            }
            For => {
                let s = match self.peek() {
                    Some(Tok::Kw(Count, _)) => {
                        self.pos += 1;
                        let (var, from, to) =
                            (self.need_arg()?, self.need_arg()?, self.need_arg()?);
                        Stmt::ForCount {
                            var,
                            from,
                            to,
                            step: self.arg(),
                            body: vec![],
                        }
                    }
                    Some(Tok::Kw(Each, _)) => {
                        self.pos += 1;
                        // `For Each DataFile <var> <folder> …`: function name first, then the variable.
                        let fline = self.line();
                        let name = match self.peek() {
                            Some(Tok::Func(n)) => n.clone(),
                            _ => return Err(self.unexpected()),
                        };
                        self.pos += 1;
                        let var = self.need_arg()?;
                        Stmt::ForEach {
                            var,
                            func: Func {
                                name,
                                args: self.args(),
                                line: fline,
                            },
                            body: vec![],
                        }
                    }
                    _ => return Err(self.unexpected()),
                };
                let body = self.block()?;
                self.kw(EndFor)?;
                match s {
                    Stmt::ForCount {
                        var,
                        from,
                        to,
                        step,
                        ..
                    } => Stmt::ForCount {
                        var,
                        from,
                        to,
                        step,
                        body,
                    },
                    Stmt::ForEach { var, func, .. } => Stmt::ForEach { var, func, body },
                    _ => unreachable!(),
                }
            }
            _ => {
                self.pos -= 1;
                return Err(self.unexpected());
            }
        })
    }
}

// ---------- running ----------

enum Halt {
    Ask(Prompt),
    Fail(String),
}
type H<T> = Result<T, Halt>;

fn fail<T>(msg: impl Into<String>) -> H<T> {
    Err(Halt::Fail(msg.into()))
}

/// What a function gives back. Scripts only read results in `If` and `For Each`, so string
/// results (C# returns them) are dropped.
enum Val {
    None,
    Bool(bool),
    List(Vec<String>),
}

struct Interp<'a> {
    host: &'a Host,
    answers: &'a [Answer],
    asked: usize,
    /// Lowercase name -> value (None = C# null).
    vars: HashMap<String, Option<String>>,
    plan: Plan,
    /// Lowercase mod paths `CompleteFileInstallation` skips.
    excluded: HashSet<String>,
    install_data: bool,
    install_plugins: bool,
    /// `LoadBefore`/`LoadAfter`/`LoadEarly` order, applied at completion.
    order: Vec<String>,
    /// Lowercase mod path -> where it was copied (C# `m_dicCopiedFileMappings`).
    copied: HashMap<String, String>,
    /// Lowercase mod path -> actual path, any file in the archive.
    index: HashMap<String, String>,
    for_depth: u32,
    select_depth: u32,
    cont: bool,
    exit: bool,
    brk: bool,
    ret: bool,
    fatal: bool,
    goto: bool,
    goto_label: String,
    find_label: bool,
}

/// Mod/data path as scripts write it: trimmed, '/'-separated, no leading '/'.
fn norm(p: &str) -> String {
    p.trim()
        .replace('\\', "/")
        .trim_start_matches('/')
        .to_string()
}

fn file_name(p: &str) -> &str {
    p.rsplit(['/', '\\']).next().unwrap_or(p)
}

/// C# `Path.GetDirectoryName`, '/'-separated; "" for none.
fn dir_name(p: &str) -> &str {
    p.rfind(['/', '\\']).map_or("", |i| &p[..i])
}

fn parse_bool(s: &str) -> H<bool> {
    match s.trim().to_ascii_lowercase().as_str() {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => fail(format!("'{s}' is not true or false")),
    }
}

fn parse_num<T: std::str::FromStr>(s: &str, what: &str) -> H<T> {
    let t = s.trim();
    t.strip_prefix('+')
        .unwrap_or(t)
        .parse()
        .or_else(|_| fail(format!("'{s}' is not {what}")))
}

/// C# `new Version(s)`, `.0` added to a bare number.
fn version(s: &str) -> H<Version> {
    let s = s.trim();
    let s = if s.contains('.') {
        s.to_string()
    } else {
        format!("{s}.0")
    };
    match parse_version(&s) {
        [0, 0, 0, 0] if s.split('.').any(|p| p.parse::<u32>().is_err()) => {
            fail(format!("'{s}' is not a version"))
        }
        v => Ok(v),
    }
}

/// Unanchored, case-sensitive `*` wildcard (C# turns it into a regex the same way).
fn wildcard(pattern: Option<&str>, text: &str) -> bool {
    let Some(p) = pattern.filter(|p| !p.is_empty()) else {
        return true;
    };
    let mut pos = 0;
    for part in p.split('*').filter(|s| !s.is_empty()) {
        match text[pos..].find(part) {
            Some(i) => pos += i + part.len(),
            None => return false,
        }
    }
    true
}

/// C# `\s` -> ' ', then runs of spaces -> one.
fn squash(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars().map(|c| if c.is_whitespace() { ' ' } else { c }) {
        if !(ch == ' ' && out.ends_with(' ')) {
            out.push(ch);
        }
    }
    out
}

/// C# `StreamReader.ReadLine` split.
fn lines_of(s: &str) -> Vec<String> {
    let mut out = vec![];
    let mut cur = String::new();
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '\r' | '\n' => {
                if c == '\r' && it.peek() == Some(&'\n') {
                    it.next();
                }
                out.push(std::mem::take(&mut cur));
            }
            _ => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

impl<'a> Interp<'a> {
    fn new(host: &'a Host, answers: &'a [Answer]) -> Self {
        let index = walkdir::WalkDir::new(&host.base)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_file())
            .filter_map(|e| {
                let r = e
                    .path()
                    .strip_prefix(&host.base)
                    .ok()?
                    .to_string_lossy()
                    .replace('\\', "/");
                Some((r.to_lowercase(), r))
            })
            .collect();
        let vars = [("newline", "\r\n"), ("tab", "\t")]
            .into_iter()
            .map(|(k, v)| (k.to_string(), Some(v.to_string())))
            .collect();
        Interp {
            host,
            answers,
            asked: 0,
            vars,
            plan: Plan::default(),
            excluded: HashSet::new(),
            install_data: true,
            install_plugins: true,
            order: vec![],
            copied: HashMap::new(),
            index,
            for_depth: 0,
            select_depth: 0,
            cont: false,
            exit: false,
            brk: false,
            ret: false,
            fatal: false,
            goto: false,
            goto_label: String::new(),
            find_label: false,
        }
    }

    fn ask(&mut self, p: Prompt) -> H<Answer> {
        match self.answers.get(self.asked) {
            Some(a) => {
                self.asked += 1;
                Ok(a.clone())
            }
            None => Err(Halt::Ask(p)),
        }
    }

    fn var(&self, name: &str) -> Option<String> {
        self.vars.get(&name.to_lowercase()).cloned().flatten()
    }
    fn set(&mut self, name: &str, v: Option<String>) {
        self.vars.insert(name.to_lowercase(), v);
    }

    /// C# `ExpandVariables`: unescape `\\`, drop unescaped quotes, substitute `%var%`.
    fn expand(&self, raw: &str) -> String {
        let s: Vec<char> = raw.replace("\\\\", "\\").chars().collect();
        let mut t = String::new();
        if s != ['"', '"'] {
            let mut i = usize::from(s.first() == Some(&'"'));
            while i < s.len() {
                t.push(s[i]);
                i += if s[i] != '\\' && s.get(i + 1) == Some(&'"') {
                    2
                } else {
                    1
                };
            }
        }
        let mut names = vec![];
        let mut rest = &t[..];
        while let Some(a) = rest.find('%') {
            match rest[a + 1..].find('%') {
                Some(0) => rest = &rest[a + 1..],
                Some(b) => {
                    names.push(rest[a + 1..a + 1 + b].to_string());
                    rest = &rest[a + b + 2..];
                }
                None => break,
            }
        }
        for n in names {
            t = t.replace(&format!("%{n}%"), &self.var(&n).unwrap_or_default());
        }
        t
    }

    fn block(&mut self, stmts: &[Stmt]) -> H<()> {
        for s in stmts {
            self.stmt(s)?;
            if self.ret || self.cont || self.exit || self.brk || self.goto {
                break;
            }
        }
        Ok(())
    }

    fn stmt(&mut self, s: &Stmt) -> H<()> {
        if self.find_label && !matches!(s, Stmt::Label(_)) {
            // C#: looking for a Goto's label, only descend into nested blocks.
            match s {
                Stmt::If { then, els, .. } => {
                    self.block(then)?;
                    if self.find_label {
                        self.block(els)?;
                    }
                }
                Stmt::Select { cases, .. } | Stmt::SelectStr { cases, .. } => {
                    for c in cases {
                        self.block(&c.body)?;
                        if !self.find_label {
                            break;
                        }
                    }
                }
                Stmt::ForCount { body, .. } | Stmt::ForEach { body, .. } => self.block(body)?,
                _ => {}
            }
            return Ok(());
        }
        match s {
            Stmt::Call(f) => {
                self.call(f)?;
            }
            Stmt::Goto(l) => {
                self.goto = true;
                self.goto_label = self.expand(l);
            }
            Stmt::Label(l) => {
                if self.find_label {
                    self.find_label = self.goto_label != self.expand(l);
                }
            }
            Stmt::ExecLines(code) => {
                let code = self.expand(code);
                match parse(&preprocess(&code)) {
                    Ok(b) => self.block(&b)?,
                    Err(e) => return fail(format!("Invalid Mod Script in ExecLines: {e}")),
                }
            }
            Stmt::Return => self.ret = true,
            Stmt::Fatal => {
                self.ret = true;
                self.fatal = true;
            }
            Stmt::Break(line) => {
                if self.select_depth == 0 {
                    return fail(format!("line {line}: Break outside a Select"));
                }
                self.brk = true;
            }
            Stmt::Continue(line) | Stmt::Exit(line) => {
                if self.for_depth == 0 {
                    return fail(format!("line {line}: Continue/Exit outside a For loop"));
                }
                match s {
                    Stmt::Continue(_) => self.cont = true,
                    _ => self.exit = true,
                }
            }
            Stmt::If {
                not,
                cond,
                then,
                els,
            } => {
                let Val::Bool(b) = self.call(cond)? else {
                    return fail(format!(
                        "line {}: {} doesn't give true or false",
                        cond.line, cond.name
                    ));
                };
                if b != *not {
                    self.block(then)?;
                } else {
                    self.block(els)?;
                }
            }
            Stmt::Select {
                kind,
                title,
                options,
                cases,
            } => self.select(*kind, title, options, cases)?,
            Stmt::SelectStr { var, expr, cases } => {
                self.select_depth += 1;
                let e = self.expand(expr);
                let e = if *var { self.var(&e) } else { Some(e) };
                if self.run_cases(e.map(|e| squash(&e)).as_deref(), cases)? {
                    self.select_depth -= 1;
                }
            }
            Stmt::ForCount {
                var,
                from,
                to,
                step,
                body,
            } => {
                let var = self.expand(var);
                let from: i64 = parse_num(&self.expand(from), "a whole number")?;
                let to: i64 = parse_num(&self.expand(to), "a whole number")?;
                let step: i64 = match step {
                    Some(s) => parse_num(&self.expand(s), "a whole number")?,
                    None => 1,
                };
                if step <= 0 {
                    return fail("For Count needs a positive step");
                }
                let items = (from..=to).step_by(step as usize).map(|i| i.to_string());
                self.for_loop(&var, items.collect(), body)?;
            }
            Stmt::ForEach { var, func, body } => {
                let var = self.expand(var);
                let Val::List(items) = self.call(func)? else {
                    return fail(format!(
                        "line {}: {} doesn't give a list",
                        func.line, func.name
                    ));
                };
                self.for_loop(&var, items, body)?;
            }
        }
        Ok(())
    }

    fn for_loop(&mut self, var: &str, items: Vec<String>, body: &[Stmt]) -> H<()> {
        self.for_depth += 1;
        for item in items {
            self.set(var, Some(item));
            self.block(body)?;
            self.cont = false;
            if self.ret || self.goto {
                return Ok(());
            }
            if self.exit {
                break;
            }
        }
        self.exit = false;
        self.for_depth -= 1;
        Ok(())
    }

    /// Run the case matching `expr`, falling through to later ones until a `Break`, then
    /// `Default` unless a matched case broke (C#). Returns false when stopped by Return/Goto.
    fn run_cases(&mut self, expr: Option<&str>, cases: &[Case]) -> H<bool> {
        let default = cases.iter().rev().find(|c| c.matches.is_none());
        let mut matched = false;
        for c in cases {
            let Some(m) = &c.matches else { continue };
            let key = m
                .iter()
                .map(|a| self.expand(a))
                .collect::<Vec<_>>()
                .join(" ");
            if matched || expr == Some(&key[..]) {
                matched = true;
                self.block(&c.body)?;
                if self.ret || self.goto {
                    return Ok(false);
                }
                if self.brk {
                    break;
                }
            }
        }
        if let Some(d) = default.filter(|_| !matched || !self.brk) {
            self.block(&d.body)?;
            if self.ret || self.goto {
                return Ok(false);
            }
        }
        self.brk = false;
        Ok(true)
    }

    fn select(&mut self, kind: Kw, title: &str, opts: &[Vec<String>], cases: &[Case]) -> H<()> {
        use Kw::*;
        self.select_depth += 1;
        let title = self.expand(title);
        let many = matches!(
            kind,
            SelectMany
                | SelectManyWithPreview
                | SelectManyWithDescriptions
                | SelectManyWithDescriptionsAndPreviews
        );
        let none = |s: Option<String>| s.filter(|s| !s.eq_ignore_ascii_case("none"));
        let mut options: Vec<SelectOption> =
            opts.iter()
                .map(|o| {
                    let name = self.expand(&o[0]);
                    let p = |i: usize| o.get(i).map(|a| self.expand(a));
                    let (description, image) = match kind {
                        SelectWithDescriptions | SelectManyWithDescriptions => (p(1), None),
                        SelectWithPreview | SelectManyWithPreview => (None, none(p(1))),
                        SelectWithDescriptionsAndPreviews
                        | SelectManyWithDescriptionsAndPreviews => (p(1), none(p(2))),
                        _ => (None, None),
                    };
                    let default = name.starts_with('|');
                    SelectOption {
                        name: name.strip_prefix('|').unwrap_or(&name).to_string(),
                        description,
                        image,
                        default,
                    }
                })
                .collect();
        if !many && !options.iter().any(|o| o.default) {
            options[0].default = true;
        }
        let names: Vec<String> = options.iter().map(|o| o.name.clone()).collect();
        let picked = match self.ask(Prompt::Select {
            title,
            many,
            options,
        })? {
            Answer::Pick(p) if p.iter().all(|&i| i < names.len()) && (many || p.len() == 1) => p,
            _ => return fail("answer doesn't fit the Select"),
        };
        for &i in &picked {
            if !self.run_cases(Some(&squash(&names[i])), cases)? {
                return Ok(());
            }
        }
        if picked.is_empty() {
            if let Some(d) = cases.iter().rev().find(|c| c.matches.is_none()) {
                self.block(&d.body)?;
            }
            self.brk = false;
        }
        self.select_depth -= 1;
        Ok(())
    }

    // ---------- functions ----------

    fn call(&mut self, f: &Func) -> H<Val> {
        let a: Vec<String> = f.args.iter().map(|x| self.expand(x)).collect();
        let gb = self.host.gamebryo;
        let opt = |i: usize| a.get(i).cloned();
        let s = |_: String| -> H<Val> { Ok(Val::None) };
        let b = |v: bool| -> H<Val> { Ok(Val::Bool(v)) };
        let none = || -> H<Val> { Ok(Val::None) };
        match (f.name.to_ascii_lowercase().as_str(), a.len()) {
            // C# ModScriptInterpreterContext: functions storing into a variable come first.
            ("getvar", 1) => none(),
            ("setvar", 2) => {
                self.set(&a[0], Some(a[1].clone()));
                none()
            }
            ("substring" | "removestring", 3 | 4) => {
                let v = self.substring(&f.name, &a[1..])?;
                self.set(&a[0], Some(v));
                none()
            }
            ("stringlength", 2) => {
                self.set(&a[0], Some(a[1].chars().count().to_string()));
                none()
            }
            ("combinepaths", 3) => {
                self.set(&a[0], Some(combine(&a[1], &a[2])));
                none()
            }
            ("getdirectoryname", 2) => {
                self.set(&a[0], Some(dir_name(&a[1]).replace('/', "\\")));
                none()
            }
            ("getfilename", 2) => {
                self.set(&a[0], Some(file_name(&a[1]).into()));
                none()
            }
            ("getfilenamewithoutextension", 2) => {
                self.set(&a[0], Some(stem(&a[1]).into()));
                none()
            }
            ("inputstring", 1..=3) => {
                let v = self.input(opt(1), opt(2))?;
                self.set(&a[0], v);
                none()
            }
            ("readrendererinfo", 2) if gb => {
                let v = self.renderer(&a[1])?;
                self.set(&a[0], v);
                none()
            }
            ("completefileinstallation", 0) => {
                self.complete()?;
                none()
            }
            ("dontinstallanydatafiles", 0) => {
                self.install_data = false;
                none()
            }
            ("dontinstallanyplugins", 0) => {
                self.install_plugins = false;
                none()
            }
            ("installalldatafiles", 0) => {
                self.install_all(false)?;
                none()
            }
            ("installallplugins", 0) => {
                self.install_all(true)?;
                none()
            }
            ("dontinstallmodfolder" | "dontinstalldatafolder", 1 | 2) => {
                let recurse = match opt(1) {
                    Some(r) => parse_bool(&r)?,
                    None => false,
                };
                for f in self.list(&a[0], recurse) {
                    self.excluded.insert(f.to_lowercase());
                }
                none()
            }
            ("dontinstallmodfile" | "dontinstalldatafile" | "dontinstallplugin", 1) => {
                self.excluded.insert(norm(&a[0]).to_lowercase());
                none()
            }
            ("loadbefore", 2) => {
                self.load_before(&a[0], &a[1]);
                none()
            }
            ("loadafter", 2) => {
                self.load_after(&a[0], &a[1]);
                none()
            }
            ("loadearly", 1) => {
                let name = file_name(&a[0]).to_string();
                if self.in_mod_root(&name) {
                    if let Some(i) = self.order.iter().position(|p| p == &a[0]) {
                        self.order.remove(i);
                    }
                    self.order.insert(0, name);
                }
                none()
            }
            ("iset" | "fset", n) if n >= 1 => {
                let expr = a[1..].join(" ").replace("mod", "%");
                let v = calculate(&expr)?;
                self.set(&a[0], Some(v));
                none()
            }

            // C# ModScriptFunctionProxy.
            ("installfilefrommod" | "installplugin" | "installdatafile", 1) => {
                b(self.install(&a[0], &a[0])?)
            }
            ("installfilefrommod" | "copydatafile" | "copyplugin", 2) => {
                b(self.install(&a[0], &a[1])?)
            }
            ("patchplugin" | "patchdatafile", 3) => {
                if parse_bool(&a[2])? || self.data_exists(&a[1]) {
                    self.install(&a[0], &a[1])?;
                }
                none()
            }
            ("installdatafolder", 2) => {
                let from = norm(&a[0]).to_lowercase();
                b(self.install_folder(&from, &from, parse_bool(&a[1])?)?)
            }
            ("copydatafolder", 3) => {
                b(self.install_folder(&norm(&a[0]).to_lowercase(), &a[1], parse_bool(&a[2])?)?)
            }
            ("datafolder" | "pluginfolder", 1..=3) => {
                let recurse = parse_bool(opt(1).as_deref().unwrap_or("false"))?;
                let data = f.name.eq_ignore_ascii_case("datafolder");
                Ok(Val::List(self.folders(
                    &a[0],
                    recurse,
                    opt(2).as_deref(),
                    data,
                )))
            }
            ("datafile" | "plugin", 1..=3) => {
                let recurse = parse_bool(opt(1).as_deref().unwrap_or("false"))?;
                let plugins = f.name.eq_ignore_ascii_case("plugin");
                let mut out: Vec<String> = vec![];
                for file in self.list(&a[0], recurse) {
                    if wildcard(opt(2).as_deref(), &file)
                        && self.is_plugin(&file) == plugins
                        && !out.iter().any(|o| o.eq_ignore_ascii_case(&file))
                    {
                        out.push(file);
                    }
                }
                Ok(Val::List(out))
            }
            (
                n @ ("setfilebyte" | "setfileshort" | "setfileint" | "setfilelong" | "setfilefloat"
                | "setpluginbyte" | "setpluginshort" | "setpluginint" | "setpluginlong"
                | "setpluginfloat"),
                3,
            ) => {
                let off: u64 = parse_num(&a[1], "a file offset")?;
                let v = &a[2];
                let bytes = match &n[n.len() - 4..] {
                    "byte" => vec![parse_num::<u8>(v, "a byte")?],
                    "hort" => parse_num::<i16>(v, "a short")?.to_le_bytes().to_vec(),
                    "eint" | "nint" => parse_num::<i32>(v, "an integer")?.to_le_bytes().to_vec(),
                    "long" => parse_num::<i64>(v, "a long")?.to_le_bytes().to_vec(),
                    _ => parse_num::<f32>(v, "a number")?.to_le_bytes().to_vec(),
                };
                let mut file = self.existing(&a[0])?;
                let off = off as usize;
                if off + bytes.len() > file.len() {
                    return fail(format!("offset {off} is past the end of '{}'", a[0]));
                }
                file[off..off + bytes.len()].copy_from_slice(&bytes);
                self.plan.put(norm(&a[0]), Src::Bytes(file));
                none()
            }
            ("settextline" | "editxmlline", 3) => {
                let path = self.copied_to(&a[0]);
                let n: usize = parse_num(&a[1], "a line number")?;
                let mut lines = lines_of(&decode(&self.existing(&path)?));
                let Some(l) = lines.get_mut(n) else {
                    return fail(format!("'{path}' has no line {n}"));
                };
                *l = a[2].clone();
                let mut text = lines.join("\r\n");
                text.push_str("\r\n");
                self.plan.put(norm(&path), Src::Bytes(text.into_bytes()));
                none()
            }
            ("replacetextinfile" | "editxmlreplace", 3) => {
                if a[1].is_empty() {
                    return fail("ReplaceTextInFile: nothing to replace");
                }
                let path = self.copied_to(&a[0]);
                let text = decode(&self.existing(&path)?).replace(&a[1], &a[2]);
                self.plan.put(norm(&path), Src::Bytes(text.into_bytes()));
                none()
            }
            ("combinepaths", 2) => s(combine(&a[0], &a[1])),
            ("getdirectoryname", 1) => s(dir_name(&a[0]).replace('/', "\\")),
            ("getfilename", 1) => s(file_name(&a[0]).into()),
            ("getfilenamewithoutextension", 1) => s(stem(&a[0]).into()),
            ("dialogyesno", 1 | 2) => {
                let title = opt(1).unwrap_or_else(|| "Question".into());
                match self.ask(Prompt::YesNo {
                    title,
                    message: a[0].clone(),
                })? {
                    Answer::Bool(v) => b(v),
                    _ => fail("answer doesn't fit DialogYesNo"),
                }
            }
            ("message" | "messagebox", 1 | 2) => {
                self.ask(Prompt::Message {
                    title: opt(1),
                    message: a[0].clone(),
                })?;
                none()
            }
            ("inputstring", 0) => {
                self.input(None, None)?;
                none()
            }
            ("displaytext", 1 | 2) => {
                let text = decode(&self.read_mod(&a[0])?);
                self.ask(Prompt::Text {
                    title: opt(1),
                    text,
                })?;
                none()
            }
            ("displayimage", 2) => {
                let path = self.mod_path(&a[0])?;
                self.ask(Prompt::Image {
                    title: opt(1),
                    path,
                })?;
                none()
            }
            // C# reports 1.12 to scripts at least, which NMM's own version never reached.
            ("versionlessthan", 1) => b([1, 12, 0, 0] < version(&a[0])?),
            ("versiongreaterthan", 1) => b([1, 12, 0, 0] > version(&a[0])?),
            ("gamenewerthan", 1) => b(self.host.game > Some(version(&a[0])?)),
            ("oblivionnewerthan" | "morrowindnewerthan", 1) if gb => {
                b(self.host.game > Some(version(&a[0])?))
            }
            ("scriptextendernewerthan", 1) => match gb {
                true => b(self.host.extender > Some(version(&a[0])?)),
                false => b(true),
            },
            ("scriptextenderpresent", 0) if gb => b(self.host.extender.is_some()),
            ("isplugin", 1) => b(self.is_plugin(&a[0])),
            ("uncheckesp", 1) => {
                self.plan.uncheck.push(a[0].clone());
                none()
            }
            ("equal", 2) => b(a[0] == a[1]),
            ("greaterthan" | "greaterequal", 2) => {
                let x: i32 = parse_num(&a[0], "an integer")?;
                let y: i32 = parse_num(&a[1], "an integer")?;
                b(if f.name.eq_ignore_ascii_case("greaterthan") {
                    x > y
                } else {
                    x >= y
                })
            }
            ("fgreaterthan" | "fgreaterequal", 2) => {
                let x: f32 = parse_num(&a[0], "a number")?;
                let y: f32 = parse_num(&a[1], "a number")?;
                b(if f.name.eq_ignore_ascii_case("fgreaterthan") {
                    x > y
                } else {
                    x >= y
                })
            }
            ("substring" | "removestring", 2) => s(self.substring(&f.name, &a)?),
            ("stringlength", 1) => s(a[0].chars().count().to_string()),
            ("calculate", 1) => s(calculate(&a[0].replace("mod", "%"))?),
            ("performbasicinstall", 0) => {
                for f in self.host.files.clone() {
                    self.install(&f, &f)?;
                }
                b(true)
            }
            ("getmodfilelist", 0) => Ok(Val::List(self.host.files.clone())),
            ("datafileexists", 1) => b(self.data_exists(&a[0])),
            ("getallplugins", 0) => Ok(Val::List(
                self.host.plugins.iter().map(|p| p.0.clone()).collect(),
            )),
            ("getactiveplugins", 0) => Ok(Val::List(
                self.host
                    .plugins
                    .iter()
                    .filter(|p| p.1)
                    .map(|p| p.0.clone())
                    .collect(),
            )),
            ("setrelativeloadorder", _) => {
                if !a.is_empty() {
                    self.plan.orders.push(a.clone());
                }
                none()
            }
            ("getinistring" | "getiniint", 3) => {
                let path = self.ini_file(&a[0]);
                s(self.ini_get(&path, &a[1], &a[2]).unwrap_or_default())
            }
            ("editini", 4) => {
                let path = self.ini_file(&a[0]);
                self.plan
                    .ini
                    .push((path, a[1].clone(), a[2].clone(), a[3].clone()));
                b(true)
            }
            ("editini", 3) if gb => {
                let path = self.game_ini()?;
                self.plan
                    .ini
                    .push((path, a[0].clone(), a[1].clone(), a[2].clone()));
                b(true)
            }
            ("registerbsa" | "unregisterbsa", 1) if gb => {
                let ini = self.game_ini()?;
                let mut list: Vec<String> = self
                    .ini_get(&ini, "Archive", "SArchiveList")
                    .unwrap_or_default()
                    .split(',')
                    .map(|s| s.trim_matches(' ').to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                let bsa = a[0].trim().to_string();
                let at = list.iter().position(|x| x.eq_ignore_ascii_case(&bsa));
                match (f.name.eq_ignore_ascii_case("registerbsa"), at) {
                    (true, None) => list.push(bsa),
                    (false, Some(i)) => {
                        list.remove(i);
                    }
                    _ => return none(),
                }
                let v = list.join(", ");
                self.plan
                    .ini
                    .push((ini, "Archive".into(), "SArchiveList".into(), v));
                none()
            }
            // ponytail: shader package (.sdp) edits aren't ported; scripts see false.
            ("editshader", 3) if gb => {
                tracing::warn!(shader = %a[1], "Mod Script EditShader is not supported");
                b(false)
            }
            ("readrendererinfo", 1) if gb => {
                self.renderer(&a[0])?;
                none()
            }
            ("setdeactivationwarning", 2) => match a[1].as_str() {
                "Allow" | "WarnAgainst" | "Disallow" => none(),
                w => fail(format!("'{w}' is not Allow, WarnAgainst or Disallow")),
            },
            // C# ignores these (OBMM conflict/dependency declarations).
            ("conflictswith" | "conflictswithregex", 1 | 2 | 3 | 5 | 6 | 7) if gb => none(),
            ("dependson" | "dependsonregex", 1 | 2 | 5 | 6) if gb => none(),
            ("getgameversion", 0) => s(self.host.game.map(show).unwrap_or_default()),
            ("getmodmanagerversion", 0) => s("1.12".into()),
            ("getscriptextenderversion", 0) if gb => {
                s(self.host.extender.map(show).unwrap_or_default())
            }
            _ => fail(format!(
                "line {}: Unrecognized function {}({})",
                f.line,
                f.name,
                a.join(", ")
            )),
        }
    }

    fn substring(&self, name: &str, a: &[String]) -> H<String> {
        let chars: Vec<char> = a[0].chars().collect();
        let start: usize = parse_num(&a[1], "a position")?;
        let len = match a.get(2) {
            Some(l) => parse_num(l, "a length")?,
            None => chars.len().saturating_sub(start),
        };
        if start + len > chars.len() {
            return fail(format!("'{}' is too short for {name}", a[0]));
        }
        Ok(if name.eq_ignore_ascii_case("substring") {
            chars[start..start + len].iter().collect()
        } else {
            chars[..start].iter().chain(&chars[start + len..]).collect()
        })
    }

    fn input(&mut self, title: Option<String>, initial: Option<String>) -> H<Option<String>> {
        match self.ask(Prompt::Input { title, initial })? {
            Answer::Text(t) => Ok(Some(t)),
            Answer::Ack(()) => Ok(None),
            _ => fail("answer doesn't fit InputString"),
        }
    }

    fn renderer(&self, key: &str) -> H<Option<String>> {
        let path = self
            .host
            .renderer
            .as_ref()
            .ok_or(Halt::Fail("this game has no RendererInfo.txt".into()))?;
        let Ok(bytes) = std::fs::read(path) else {
            return fail(format!("can't read {}", path.display()));
        };
        for l in lines_of(&decode(&bytes)) {
            let mut parts = l.split(':');
            if parts.next().unwrap().trim().eq_ignore_ascii_case(key) {
                let rest: Vec<&str> = parts.collect();
                return Ok((!rest.is_empty()).then(|| rest.join(":")));
            }
        }
        Ok(None)
    }

    fn is_plugin(&self, path: &str) -> bool {
        crate::game::has_ext(path, &self.host.plugin_exts)
    }

    /// Actual path of a mod file.
    fn mod_path(&self, path: &str) -> H<String> {
        self.index
            .get(&norm(path).to_lowercase())
            .cloned()
            .ok_or_else(|| Halt::Fail(format!("the mod has no file '{path}'")))
    }

    fn read_mod(&self, path: &str) -> H<Vec<u8>> {
        let p = self.host.base.join(self.mod_path(path)?);
        std::fs::read(&p).or_else(|e| fail(format!("can't read {}: {e}", p.display())))
    }

    fn data_exists(&self, path: &str) -> bool {
        let p = norm(path);
        self.plan.get(&p).is_some()
            || crate::manager::resolve_ci(&self.host.install_root, &p).is_file()
    }

    /// A data file as the game will see it: written or installed by this script, else on disk,
    /// else the mod's own file at that path (OBMM edits data files after copying them).
    fn existing(&self, path: &str) -> H<Vec<u8>> {
        let p = norm(path);
        match self.plan.get(&p) {
            Some(Src::Bytes(b)) => return Ok(b.clone()),
            Some(Src::Mod(m)) => return self.read_mod(m),
            None => {}
        }
        if let Ok(b) = std::fs::read(crate::manager::resolve_ci(&self.host.install_root, &p)) {
            return Ok(b);
        }
        self.read_mod(&p)
            .map_err(|_| Halt::Fail(format!("data file '{path}' doesn't exist")))
    }

    fn copied_to(&self, path: &str) -> String {
        self.copied
            .get(&norm(path).to_lowercase())
            .cloned()
            .unwrap_or_else(|| path.to_string())
    }

    fn install(&mut self, from: &str, to: &str) -> H<bool> {
        let src = self.mod_path(from)?;
        let to_n = norm(to);
        if to_n.is_empty() {
            return fail(format!("no destination for '{from}'"));
        }
        if !norm(from).eq_ignore_ascii_case(&to_n) {
            self.copied.insert(norm(from).to_lowercase(), to_n.clone());
        }
        self.plan.put(to_n, Src::Mod(src));
        Ok(true)
    }

    /// C# `InstallFolderFromMod`: `from` already normalised + lowercased.
    fn install_folder(&mut self, from: &str, to: &str, recurse: bool) -> H<bool> {
        let from_len = if from.is_empty() { 0 } else { from.len() + 1 };
        let to = norm(to);
        let to = to.trim_end_matches('/');
        for f in self.list(from, recurse) {
            let rest = &f[from_len..];
            let dest = if to.is_empty() {
                rest.to_string()
            } else {
                format!("{to}/{rest}")
            };
            self.install(&f, &dest)?;
        }
        Ok(true)
    }

    /// Mod files in `folder` (C# `Mod.GetFileList(folder, recurse)`).
    fn list(&self, folder: &str, recurse: bool) -> Vec<String> {
        let f = norm(folder).trim_end_matches('/').to_lowercase();
        let prefix = if f.is_empty() { f } else { f + "/" };
        self.host
            .files
            .iter()
            .filter(|p| {
                let l = p.to_lowercase();
                l.starts_with(&prefix) && (recurse || !l[prefix.len()..].contains('/'))
            })
            .cloned()
            .collect()
    }

    /// C# `GetFolders`: folders holding data (or plugin) files; without `recurse`, only the
    /// direct subfolders of `folder`. Backslash-separated, as C# gives them.
    fn folders(
        &self,
        folder: &str,
        recurse: bool,
        search: Option<&str>,
        data: bool,
    ) -> Vec<String> {
        let root = norm(folder).trim_end_matches('/').to_string();
        let mut out: Vec<String> = vec![];
        for f in self.list(&root, true) {
            let mut dir = dir_name(&f);
            if !recurse {
                let mut parent = dir_name(dir);
                while !parent.is_empty() && !parent.eq_ignore_ascii_case(&root) {
                    dir = parent;
                    parent = dir_name(dir);
                }
                // C# never matches the mod root here; we do.
                if !parent.eq_ignore_ascii_case(&root) || dir.is_empty() {
                    continue;
                }
            }
            let dir = dir.replace('/', "\\");
            if wildcard(search, &dir)
                && (self.is_plugin(&f) != data)
                && !out.iter().any(|o| o.eq_ignore_ascii_case(&dir))
            {
                out.push(dir);
            }
        }
        out
    }

    /// C# `CompleteFileInstallation`. Files the script already wrote keep the written version.
    fn complete(&mut self) -> H<()> {
        if self.install_data {
            self.install_all(false)?;
        }
        if self.install_plugins {
            self.install_all(true)?;
        }
        if !self.order.is_empty() {
            self.plan.orders.push(self.order.clone());
        }
        Ok(())
    }

    fn install_all(&mut self, plugins: bool) -> H<()> {
        for f in self.host.files.clone() {
            if self.is_plugin(&f) != plugins
                || self.excluded.contains(&f.to_lowercase())
                || matches!(self.plan.get(&f), Some(Src::Bytes(_)))
            {
                continue;
            }
            self.install(&f, &f)?;
        }
        Ok(())
    }

    /// C# checks plugin names against the mod's file list as given: only root files match.
    fn in_mod_root(&self, name: &str) -> bool {
        self.host.files.iter().any(|f| f.eq_ignore_ascii_case(name))
    }

    fn pos(&self, name: &str) -> Option<usize> {
        self.order.iter().position(|p| p.eq_ignore_ascii_case(name))
    }

    fn load_before(&mut self, first: &str, second: &str) {
        let first = file_name(first).to_string();
        if !self.in_mod_root(&first) {
            return;
        }
        let second = file_name(second).to_string();
        let (fp, sp) = (self.pos(&first), self.pos(&second));
        if let Some(fp) = fp {
            if sp.is_some_and(|sp| fp <= sp) {
                return;
            }
            if let Some(sp) = sp {
                self.order.remove(sp);
            }
            let at = (fp + 1).min(self.order.len());
            self.order.insert(at, second);
            return;
        }
        let sp = sp.unwrap_or_else(|| {
            self.order.push(second);
            self.order.len() - 1
        });
        self.order.insert(sp, first);
    }

    /// Load `second` after `first`.
    fn load_after(&mut self, second_raw: &str, first_raw: &str) {
        let second = file_name(second_raw).to_string();
        if !self.in_mod_root(&second) {
            return;
        }
        let first = file_name(first_raw).to_string();
        let (fp, sp) = (self.pos(&first), self.pos(&second));
        if let Some(sp) = sp {
            if fp.is_none_or(|fp| fp <= sp) {
                return;
            }
            self.order.remove(fp.unwrap());
            self.order.insert(sp, first);
            return;
        }
        let fp = fp.unwrap_or_else(|| {
            self.order.push(first_raw.to_string());
            self.order.len() - 1
        });
        self.order.insert(fp + 1, second);
    }

    fn game_ini(&self) -> H<PathBuf> {
        self.host
            .game_ini
            .clone()
            .ok_or_else(|| Halt::Fail("this game has no INI file".into()))
    }

    /// A bare INI name is taken to live next to the game INI.
    fn ini_file(&self, name: &str) -> PathBuf {
        let p = Path::new(name);
        match (p.parent(), &self.host.game_ini) {
            (Some(d), Some(ini)) if d.as_os_str().is_empty() && !name.contains('\\') => {
                ini.parent().unwrap_or(Path::new("")).join(name)
            }
            _ => PathBuf::from(name),
        }
    }

    /// An INI value as earlier edits in this script leave it.
    fn ini_get(&self, path: &Path, section: &str, key: &str) -> Option<String> {
        let edited = self.plan.ini.iter().rev().find(|e| {
            e.0 == path && e.1.eq_ignore_ascii_case(section) && e.2.eq_ignore_ascii_case(key)
        });
        match edited {
            Some(e) => Some(e.3.clone()),
            None => crate::ini::get(path, section, key),
        }
    }
}

fn show(v: Version) -> String {
    v.map(|n| n.to_string()).join(".")
}

/// C# `Path.Combine` with Windows separators.
fn combine(a: &str, b: &str) -> String {
    if a.is_empty() || b.starts_with(['/', '\\']) || b.get(1..2) == Some(":") {
        return b.into();
    }
    if a.ends_with(['/', '\\']) {
        format!("{a}{b}")
    } else {
        format!("{a}\\{b}")
    }
}

fn stem(p: &str) -> &str {
    let n = file_name(p);
    n.rfind('.').map_or(n, |i| &n[..i])
}

// ---------- arithmetic (C# uses NCalc) ----------

#[derive(Clone, Copy)]
enum Num {
    I(i64),
    F(f64),
}

impl Num {
    fn f(self) -> f64 {
        match self {
            Num::I(i) => i as f64,
            Num::F(f) => f,
        }
    }
}

// ponytail: + - * / % and parentheses; NCalc's functions and comparisons aren't ported.
fn calculate(expr: &str) -> H<String> {
    let c: Vec<char> = expr.chars().filter(|c| !c.is_whitespace()).collect();
    let mut pos = 0;
    let bad = || Halt::Fail(format!("can't calculate '{expr}'"));
    let v = sum(&c, &mut pos).ok_or_else(bad)?;
    if pos != c.len() {
        return Err(bad());
    }
    Ok(match v {
        Num::I(i) => i.to_string(),
        Num::F(f) if f.is_nan() => "NaN".into(),
        Num::F(f) if f.is_infinite() => if f > 0.0 { "Infinity" } else { "-Infinity" }.into(),
        // .NET shows 15 significant digits.
        Num::F(f) => format!("{}", format!("{f:.14e}").parse::<f64>().unwrap()),
    })
}

fn sum(c: &[char], pos: &mut usize) -> Option<Num> {
    let mut v = product(c, pos)?;
    while let Some(&op @ ('+' | '-')) = c.get(*pos) {
        *pos += 1;
        let r = product(c, pos)?;
        v = match (v, r, op) {
            (Num::I(a), Num::I(b), '+') => Num::I(a.checked_add(b)?),
            (Num::I(a), Num::I(b), _) => Num::I(a.checked_sub(b)?),
            (a, b, '+') => Num::F(a.f() + b.f()),
            (a, b, _) => Num::F(a.f() - b.f()),
        };
    }
    Some(v)
}

fn product(c: &[char], pos: &mut usize) -> Option<Num> {
    let mut v = unary(c, pos)?;
    while let Some(&op @ ('*' | '/' | '%')) = c.get(*pos) {
        *pos += 1;
        let r = unary(c, pos)?;
        v = match (v, r, op) {
            (Num::I(a), Num::I(b), '*') => Num::I(a.checked_mul(b)?),
            (Num::I(a), Num::I(b), '%') => Num::I(a.checked_rem(b)?),
            (a, b, '*') => Num::F(a.f() * b.f()),
            (a, b, '%') => Num::F(a.f() % b.f()),
            // NCalc divides as doubles.
            (a, b, _) => Num::F(a.f() / b.f()),
        };
    }
    Some(v)
}

fn unary(c: &[char], pos: &mut usize) -> Option<Num> {
    match c.get(*pos)? {
        '-' => {
            *pos += 1;
            Some(match unary(c, pos)? {
                Num::I(i) => Num::I(-i),
                Num::F(f) => Num::F(-f),
            })
        }
        '(' => {
            *pos += 1;
            let v = sum(c, pos)?;
            (c.get(*pos) == Some(&')')).then(|| *pos += 1)?;
            Some(v)
        }
        _ => {
            let n = c[*pos..]
                .iter()
                .take_while(|c| c.is_ascii_digit() || **c == '.')
                .count();
            let s: String = c[*pos..*pos + n].iter().collect();
            *pos += n;
            if s.contains('.') {
                s.parse().ok().map(Num::F)
            } else {
                s.parse().ok().map(Num::I)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn host(files: &[(&str, &str)]) -> (tempfile::TempDir, Host) {
        let t = tempfile::tempdir().unwrap();
        let base = t.path().join("mod");
        for (p, c) in files {
            let f = base.join(p);
            fs::create_dir_all(f.parent().unwrap()).unwrap();
            fs::write(f, c).unwrap();
        }
        let data = t.path().join("Data");
        fs::create_dir_all(&data).unwrap();
        let h = Host {
            files: files
                .iter()
                .map(|f| f.0.to_string())
                .filter(|f| !f.to_lowercase().starts_with("fomod"))
                .collect(),
            base,
            install_root: data,
            plugin_exts: vec![".esp".into(), ".esm".into()],
            plugins: vec![("Oblivion.esm".into(), true), ("Off.esp".into(), false)],
            game: Some([1, 2, 0, 416]),
            extender: None,
            game_ini: Some(t.path().join("Oblivion.ini")),
            renderer: Some(t.path().join("RendererInfo.txt")),
            gamebryo: true,
        };
        (t, h)
    }

    fn done(code: &str, h: &Host, answers: &[Answer]) -> Plan {
        match run(code, h, answers) {
            Ok(Outcome::Done(p)) => p,
            Ok(Outcome::Ask(p)) => panic!("asked {p:?}"),
            Err(e) => panic!("{e}"),
        }
    }

    fn dests(p: &Plan) -> Vec<&str> {
        p.files.values().map(|f| &f.0[..]).collect()
    }

    #[test]
    fn plain_script_installs_everything() {
        let (_t, h) = host(&[
            ("a.esp", ""),
            ("meshes/x.nif", ""),
            ("fomod/script.txt", ""),
        ]);
        let p = done("; nothing to do\nMessage \"Hi\"", &h, &[Answer::Ack(())]);
        assert_eq!(dests(&p), ["a.esp", "meshes/x.nif"]);
    }

    #[test]
    fn select_cases_fall_through_until_break() {
        let (_t, h) = host(&[("a.esp", ""), ("b.esp", ""), ("c.esp", "")]);
        let code = r#"
DontInstallAnyPlugins
Select "Pick one" "|Option A" "Option B"
Case Option A
    InstallPlugin a.esp
Case Option B
    InstallPlugin b.esp
    Break
Default
    InstallPlugin c.esp
EndSelect
"#;
        // The prompt carries the options, A preselected.
        let Ok(Outcome::Ask(Prompt::Select { options, many, .. })) = run(code, &h, &[]) else {
            panic!()
        };
        assert!(!many && options[0].default && options[0].name == "Option A");
        // A falls through into B, which breaks: Default skipped.
        assert_eq!(
            dests(&done(code, &h, &[Answer::Pick(vec![0])])),
            ["a.esp", "b.esp"]
        );
        assert_eq!(dests(&done(code, &h, &[Answer::Pick(vec![1])])), ["b.esp"]);
    }

    #[test]
    fn default_runs_when_nothing_breaks() {
        let (_t, h) = host(&[("a.esp", ""), ("c.esp", "")]);
        let code = "DontInstallAnyPlugins\nSelectString \"x\"\nCase y\nInstallPlugin a.esp\nDefault\nInstallPlugin c.esp\nEndSelect";
        assert_eq!(dests(&done(code, &h, &[])), ["c.esp"]);
    }

    #[test]
    fn select_many_runs_each_pick_and_previews_resolve() {
        let (_t, h) = host(&[("a.esp", ""), ("b.esp", ""), ("img/a.jpg", "")]);
        let code = r#"DontInstallAnyPlugins
DontInstallAnyDataFiles
SelectManyWithDescriptionsAndPreviews "Plugins" "A" "first" "img\a.jpg" "B" "second" "None"
Case A
InstallPlugin a.esp
Break
Case B
InstallPlugin b.esp
Break
EndSelect"#;
        let Ok(Outcome::Ask(Prompt::Select { options, many, .. })) = run(code, &h, &[]) else {
            panic!()
        };
        assert!(many && !options[0].default);
        assert_eq!(options[0].image.as_deref(), Some("img\\a.jpg"));
        assert_eq!(options[1].description.as_deref(), Some("second"));
        assert_eq!(options[1].image, None);
        assert_eq!(
            dests(&done(code, &h, &[Answer::Pick(vec![0, 1])])),
            ["a.esp", "b.esp"]
        );
        assert!(done(code, &h, &[Answer::Pick(vec![])]).files.is_empty());
    }

    #[test]
    fn replay_walks_prompts_in_order() {
        let (_t, h) = host(&[("a.esp", ""), ("b.esp", "")]);
        let code = r#"DontInstallAnyPlugins
If DialogYesNo "Install A?"
    InstallPlugin a.esp
EndIf
InputString name "Your name"
If Equal "%name%" "Bob"
    CopyPlugin b.esp "bob.esp"
EndIf"#;
        assert!(matches!(
            run(code, &h, &[]),
            Ok(Outcome::Ask(Prompt::YesNo { .. }))
        ));
        let a = [Answer::Bool(true)];
        assert!(matches!(
            run(code, &h, &a),
            Ok(Outcome::Ask(Prompt::Input { .. }))
        ));
        let p = done(code, &h, &[Answer::Bool(true), Answer::Text("Bob".into())]);
        assert_eq!(dests(&p), ["a.esp", "bob.esp"]);
        let p = done(code, &h, &[Answer::Bool(false), Answer::Text("Al".into())]);
        assert!(p.files.is_empty());
        // A wrong answer type is a script error, not a panic.
        assert!(run(code, &h, &[Answer::Pick(vec![0])]).is_err());
    }

    #[test]
    fn for_loops_variables_and_math() {
        let (_t, h) = host(&[
            ("textures/a/1.dds", ""),
            ("textures/b/2.dds", ""),
            ("textures/b/c/3.dds", ""),
            ("x.esp", ""),
        ]);
        let code = r#"DontInstallAnyDataFiles
DontInstallAnyPlugins
For Each DataFolder folder "textures"
    CopyDataFolder "%folder%" "out\%folder%" false
EndFor
SetVar total 0
For Count i 1 10 3
    iSet total %total% + %i%
    If GreaterThan %i% 6
        Exit
    EndIf
EndFor
CopyDataFile x.esp "sum%total%.txt"
fSet half 7 / 2
CopyDataFile x.esp "half%half%.txt""#;
        let p = done(code, &h, &[]);
        assert_eq!(
            dests(&p),
            [
                "half3.5.txt",
                "out/textures/a/1.dds",
                "out/textures/b/2.dds",
                "sum12.txt"
            ]
        );
    }

    #[test]
    fn exclusions_and_complete() {
        let (_t, h) = host(&[
            ("a.esp", ""),
            ("b.esp", ""),
            ("docs/r.txt", ""),
            ("docs/s/t.txt", ""),
        ]);
        let code = "DontInstallPlugin b.esp\nDontInstallDataFolder docs";
        assert_eq!(dests(&done(code, &h, &[])), ["a.esp", "docs/s/t.txt"]);
        let code = "DontInstallDataFolder docs true";
        assert_eq!(dests(&done(code, &h, &[])), ["a.esp", "b.esp"]);
    }

    #[test]
    fn goto_label_and_return() {
        let (_t, h) = host(&[("a.esp", ""), ("b.esp", "")]);
        let code = r#"DontInstallAnyPlugins
Goto skip
InstallPlugin a.esp
If Equal 1 1
    Label skip
    InstallPlugin b.esp
EndIf
Return
InstallPlugin a.esp"#;
        assert_eq!(dests(&done(code, &h, &[])), ["b.esp"]);
    }

    #[test]
    fn fatal_error_and_bad_scripts() {
        let (_t, h) = host(&[("a.esp", "")]);
        assert!(run("FatalError", &h, &[])
            .unwrap_err()
            .contains("FatalError"));
        assert!(run("If Equal 1 1\nInstallPlugin a.esp", &h, &[])
            .unwrap_err()
            .starts_with("Invalid Mod Script"));
        assert!(run("NoSuchThing 1 2", &h, &[])
            .unwrap_err()
            .contains("Unrecognized"));
        assert!(run("InstallPlugin missing.esp", &h, &[])
            .unwrap_err()
            .contains("missing.esp"));
        assert!(run("Break", &h, &[]).is_err());
        assert!(run("; only a comment", &h, &[]).is_err());
    }

    #[test]
    fn quotes_escapes_comments_execlines() {
        let (_t, h) = host(&[("a b.esp", ""), ("c.esp", "")]);
        let code = "DontInstallAnyPlugins ; trailing comment\nSetVar f \"a b.esp\"\nInstallPlugin \"%f%\"\nExecLines \"InstallPlugin c.esp%NewLine%SetVar x \\\"y;z\\\"\"\nIf Equal \"%x%\" \"y;z\"\nCopyPlugin c.esp semi.esp\nEndIf";
        assert_eq!(
            dests(&done(code, &h, &[])),
            ["a b.esp", "c.esp", "semi.esp"]
        );
    }

    #[test]
    fn run_on_lines_join() {
        let (_t, h) = host(&[("a.esp", "")]);
        let code = "AllowRunOnLines\nDontInstallAnyPlugins\nCopyPlugin \\\n  a.esp \\\n  b.esp";
        assert_eq!(dests(&done(code, &h, &[])), ["b.esp"]);
    }

    #[test]
    fn byte_and_text_edits_apply_to_installed_copy() {
        let (t, h) = host(&[
            ("a.esp", "ABCDEFGH"),
            ("cfg.txt", "one\r\ntwo\r\nthree\r\n"),
        ]);
        fs::write(t.path().join("Data/existing.ini"), "k=old\n").unwrap();
        let code = r#"CopyPlugin a.esp b.esp
SetPluginByte b.esp 0 97
SetPluginInt b.esp 4 1
SetTextLine cfg.txt 1 "TWO"
ReplaceTextInFile existing.ini "old" "new""#;
        let p = done(code, &h, &[]);
        let bytes = |d: &str| match p.get(d) {
            Some(Src::Bytes(b)) => b.clone(),
            other => panic!("{d}: {other:?}"),
        };
        assert_eq!(bytes("b.esp"), b"aBCD\x01\0\0\0");
        assert_eq!(bytes("cfg.txt"), b"one\r\nTWO\r\nthree\r\n");
        assert_eq!(bytes("existing.ini"), b"k=new\n");
        // Completion doesn't put the unedited mod copy back over the edited one.
        assert_eq!(p.get("a.esp"), Some(&Src::Mod("a.esp".into())));
        assert!(run("SetPluginByte a.esp 99 1", &h, &[]).is_err());
    }

    #[test]
    fn ini_bsa_versions_plugins() {
        let (t, h) = host(&[("x.bsa", ""), ("a.esp", ""), ("b.esp", "")]);
        fs::write(
            t.path().join("Oblivion.ini"),
            "[Archive]\nSArchiveList=A.bsa, B.bsa\n",
        )
        .unwrap();
        fs::write(
            t.path().join("RendererInfo.txt"),
            "Shader Package : 13\nNoValue\n",
        )
        .unwrap();
        let code = r#"EditINI General bFoo 1
RegisterBSA x.bsa
RegisterBSA X.BSA
UnregisterBSA A.bsa
If GameNewerThan 1.1
    UncheckEsp b.esp
EndIf
IfNot ScriptExtenderPresent
    LoadBefore b.esp a.esp
EndIf
ReadRendererInfo sp "shader package"
If Equal "%sp%" " 13"
    SetRelativeLoadOrder a.esp b.esp
EndIf"#;
        let p = done(code, &h, &[]);
        let ini = t.path().join("Oblivion.ini");
        let e =
            |s: &str, k: &str, v: &str| (ini.clone(), s.to_string(), k.to_string(), v.to_string());
        assert_eq!(
            p.ini,
            [
                e("General", "bFoo", "1"),
                e("Archive", "SArchiveList", "A.bsa, B.bsa, x.bsa"),
                e("Archive", "SArchiveList", "B.bsa, x.bsa"),
            ]
        );
        assert_eq!(p.uncheck, ["b.esp"]);
        assert_eq!(p.orders, [vec!["a.esp", "b.esp"], vec!["b.esp", "a.esp"]]);
        // Game-specific functions only for Oblivion/Morrowind.
        let other = Host {
            gamebryo: false,
            ..h
        };
        assert!(run("EditINI General bFoo 1", &other, &[]).is_err());
    }

    #[test]
    fn lexer_quirks() {
        let toks: Vec<Tok> = lex("If DataFile, \"a b\" x.esp %v% IfNot")
            .into_iter()
            .map(|t| t.tok)
            .collect();
        assert!(matches!(&toks[0], Tok::Kw(Kw::If, _)));
        assert_eq!(toks[1], Tok::Func("DataFile".into()));
        assert_eq!(toks[2], Tok::Comma);
        assert_eq!(toks[3], Tok::Word("\"a b\"".into()));
        assert_eq!(toks[4], Tok::Word("x.esp".into()));
        assert_eq!(toks[5], Tok::Word("%v%".into()));
        assert!(matches!(&toks[6], Tok::Kw(Kw::IfNot, _)));
    }

    #[test]
    fn calc() {
        let c = |e: &str| calculate(e).ok();
        assert_eq!(c("1 + 2 * 3").as_deref(), Some("7"));
        assert_eq!(c("(1 + 2) * 3 % 4").as_deref(), Some("1"));
        assert_eq!(c("1 / 3").as_deref(), Some("0.333333333333333"));
        assert_eq!(c("4 / 2").as_deref(), Some("2"));
        assert_eq!(c("-2.5 + 1").as_deref(), Some("-1.5"));
        assert_eq!(c("1 +"), None);
        assert_eq!(c("5 % 0"), None);
    }
}
