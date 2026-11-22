//! The window's view of a run file.
//!
//! Every record is parsed with `lenslet_core`'s own types; a call row is the
//! core [`CoreTrace`] plus a small UI sidecar (where its line lives on disk,
//! a search haystack, display keys). **All** reads of run-file fields that
//! may change between CLI versions live in this module, so a schema change
//! is fixed here and nowhere else.

use std::{
    borrow::Cow,
    collections::{BTreeSet, HashSet},
    fmt::Write as _,
    fs::File,
    io::{BufRead, Read, Seek, SeekFrom},
    ops::Deref,
    path::{Path, PathBuf},
    sync::Arc,
};

pub use lenslet_core::{
    access::Access,
    lock::Lock,
    summary::{FailureKind, Finding, Regressions, Report},
    trace::{BodyTrace as Body, HeaderTrace as Header, Message, RunHeader},
};

use crate::{fsguard, i18n::Tr, tr};

pub type CoreTrace = lenslet_core::trace::Trace;

/// The run-file format this window reads. Anything else is refused with
/// [`version_error`]. (At the 13.0 merge: `lenslet_core::trace::VERSION`.)
pub const RUN_FORMAT: u32 = 4;
pub const MAX_ROWS: usize = 5_000;
/// Bodies larger than this are shed from the in-memory row and read back from
/// the run file when the call is inspected.
pub const INLINE_BODY_LIMIT: usize = 8 * 1024;
/// Body text contributed to the search index (per request/response).
const SEARCH_BODY_BYTES: usize = 16 * 1024;

// ---------------------------------------------------------------------------
// Records
// ---------------------------------------------------------------------------

/// One line of a run file.
#[derive(Debug)]
pub enum Record {
    Run(Box<RunHeader>),
    Call(Box<Trace>),
    End(Box<Report>),
}

/// Parse one run-file line with the core schema.
pub fn parse_record(line: &str) -> Option<Record> {
    match serde_json::from_str::<lenslet_core::trace::Record>(line.trim()).ok()? {
        lenslet_core::trace::Record::Run(header) => Some(Record::Run(header)),
        lenslet_core::trace::Record::End(report) => Some(Record::End(report)),
        lenslet_core::trace::Record::Call(call) => {
            (!call.id.is_empty()).then(|| Record::Call(Box::new(Trace::new(*call))))
        }
    }
}

/// The error state for a run file this window cannot read.
pub fn version_error(version: u32) -> Option<Tr> {
    (version != RUN_FORMAT).then(|| {
        tr!(
            "This run was recorded by an older Lenslet (format {version}) — re-run your tests",
            "此运行由旧版 Lenslet 记录（格式 {version}）— 请重新运行测试"
        )
    })
}

/// The first line of a run file, when it is a header.
pub fn read_header(path: &Path) -> Option<RunHeader> {
    fsguard::assert_io_allowed();
    let mut line = String::new();
    std::io::BufReader::new(File::open(path).ok()?)
        .read_line(&mut line)
        .ok()?;
    match parse_record(&line)? {
        Record::Run(header) => Some(*header),
        _ => None,
    }
}

/// A run as the window shows it: its header, plus the report once it ended.
#[derive(Clone, Debug, Default)]
pub struct RunInfo {
    pub header: RunHeader,
    pub report: Option<Report>,
}

impl RunInfo {
    pub fn command_line(&self) -> String {
        self.header.command.join(" ")
    }

    /// The report's failure findings (the only failure list in format 4).
    pub fn failures(&self) -> Vec<&Finding> {
        self.report
            .iter()
            .flat_map(|r| r.findings.iter())
            .filter(|f| matches!(f, Finding::Failure(_)))
            .collect()
    }

    pub fn regressions(&self) -> Option<&Regressions> {
        self.report.as_ref()?.regressions.as_ref()
    }
}

/// `moto`, from the header's `emulator` (`moto (started by Lenslet)`).
pub fn emulator_name(header: &RunHeader) -> Option<String> {
    let full = emulator_full(header)?;
    let name = full.split(" (").next().unwrap_or(&full).trim().to_string();
    (!name.is_empty()).then_some(name)
}

fn emulator_full(header: &RunHeader) -> Option<String> {
    // Format 4 makes `emulator` a plain string; empty means unknown.
    let full: Option<&str> = Some(header.emulator.as_str());
    full.map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// The test command's combined output: the header's (or report's) `log`,
/// relative paths resolved against the run file's directory, else
/// `<run>.log` next to the run file.
pub fn log_path(run: &RunInfo, run_path: &Path) -> PathBuf {
    let named = run.header.log.as_deref().filter(|s| !s.is_empty());
    match named {
        Some(log) if Path::new(log).is_absolute() => PathBuf::from(log),
        Some(log) => run_path
            .parent()
            .map_or_else(|| PathBuf::from(log), |dir| dir.join(log)),
        None => lenslet_core::runs::log_path_for(run_path),
    }
}

/// The run's project directory (its `cwd`).
pub fn project_dir(header: &RunHeader) -> Option<PathBuf> {
    (!header.cwd.is_empty()).then(|| PathBuf::from(&header.cwd))
}

/// The lock a run is compared with: its header's `lock`, else
/// `cwd/lenslet.lock`.
pub fn lock_path(header: &RunHeader) -> Option<PathBuf> {
    if let Some(lock) = header.lock.as_deref().filter(|s| !s.is_empty()) {
        let path = Path::new(lock);
        return Some(if path.is_absolute() || header.cwd.is_empty() {
            path.to_path_buf()
        } else {
            Path::new(&header.cwd).join(path)
        });
    }
    project_dir(header).map(|dir| lenslet_core::lock::path_in(&dir))
}

/// The semantic target of a call (`table/orders · id="1"`). The one place
/// the window asks the core for it.
pub fn target_summary(call: &CoreTrace) -> Option<String> {
    lenslet_core::target::summary(call)
}

/// One permission a run used, as the Access view lists it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UsedAccess {
    pub access: Access,
    pub calls: usize,
    pub errors: usize,
    /// Needed by another call rather than called directly (KMS, PassRole).
    pub implied: bool,
    /// The resource is a `*` standing for one a body did not let Lenslet read.
    pub unverified: bool,
    /// The calls that needed it, e.g. `DYNAMODB DeleteItem` (first few).
    pub via: Vec<String>,
}

/// The report's access table.
pub fn report_access(report: &Report) -> Vec<UsedAccess> {
    report
        .access
        .iter()
        .map(|entry| UsedAccess {
            access: Access::new(entry.action.clone(), entry.resource.clone()),
            calls: entry.used.calls,
            errors: entry.used.errors,
            implied: entry.used.implied,
            unverified: entry.used.unverified,
            via: entry.used.via.clone(),
        })
        .collect()
}

// ---------------------------------------------------------------------------
// The lock: the one source of truth for "in lenslet.lock"
// ---------------------------------------------------------------------------

/// The lock as the window read it. With no lock file nothing is in it, so
/// every used permission reads "not in lock" everywhere.
#[derive(Clone, Debug, Default)]
pub struct LockView {
    pub path: Option<PathBuf>,
    pub lock: Option<Arc<Lock>>,
}

impl LockView {
    pub fn exists(&self) -> bool {
        self.lock.is_some()
    }

    pub fn contains(&self, access: &Access) -> bool {
        self.lock
            .as_ref()
            .is_some_and(|lock| lock.access.contains(access))
    }

    pub fn is_test(&self, access: &Access) -> bool {
        self.lock
            .as_ref()
            .is_some_and(|lock| lock.test.contains(access))
    }

    pub fn locked(&self) -> BTreeSet<Access> {
        self.lock
            .as_ref()
            .map(|lock| lock.access.clone())
            .unwrap_or_default()
    }
}

// ---------------------------------------------------------------------------
// Call rows
// ---------------------------------------------------------------------------

/// Where a row's complete line lives in its run file.
#[derive(Clone, Debug)]
pub struct LineRef {
    pub path: Arc<PathBuf>,
    pub offset: u64,
    pub len: usize,
}

/// A call as the window holds it: the core record plus display keys.
#[derive(Clone, Debug)]
pub struct Trace {
    pub core: CoreTrace,
    target: Option<String>,
    service_key: String,
    service_name: String,
    haystack: String,
    at_ms: Option<i64>,
    pub(crate) line_ref: Option<LineRef>,
    dehydrated: bool,
}

impl Deref for Trace {
    type Target = CoreTrace;
    fn deref(&self) -> &CoreTrace {
        &self.core
    }
}

impl Trace {
    pub fn new(core: CoreTrace) -> Self {
        let mut trace = Trace {
            target: target_summary(&core),
            service_key: service_id(&core.service),
            service_name: service_label(&core.service),
            at_ms: parse_rfc3339_ms(&core.at),
            core,
            haystack: String::new(),
            line_ref: None,
            dehydrated: false,
        };
        trace.haystack = trace.build_haystack();
        trace
    }

    /// Shed bodies over [`INLINE_BODY_LIMIT`] from memory, remembering where
    /// the full line is.
    pub fn dehydrate(&mut self, lref: LineRef) {
        let mut shed = false;
        for message in [&mut self.core.request, &mut self.core.response] {
            if let Some(body) = message.body.as_mut()
                && body
                    .text
                    .as_ref()
                    .is_some_and(|text| text.len() > INLINE_BODY_LIMIT)
            {
                body.text = None;
                shed = true;
            }
        }
        self.line_ref = Some(lref);
        self.dehydrated = shed;
    }

    pub fn is_dehydrated(&self) -> bool {
        self.dehydrated
    }

    /// The row with its full bodies, read back from the run file. Blocking IO:
    /// background only.
    pub fn hydrated(&self) -> Option<Trace> {
        fsguard::assert_io_allowed();
        if !self.dehydrated {
            return Some(self.clone());
        }
        let lref = self.line_ref.as_ref()?;
        let mut file = File::open(&*lref.path).ok()?;
        file.seek(SeekFrom::Start(lref.offset)).ok()?;
        let mut buf = vec![0u8; lref.len];
        file.read_exact(&mut buf).ok()?;
        match parse_record(&String::from_utf8_lossy(&buf))? {
            Record::Call(full) if full.id == self.id => Some(*full),
            _ => None,
        }
    }

    pub fn at_ms(&self) -> Option<i64> {
        self.at_ms
    }

    /// Normalized service identity (e.g. `dynamodb`).
    pub fn service_key(&self) -> &str {
        &self.service_key
    }

    /// Display label for the service (e.g. `DYNAMODB`).
    pub fn service_name(&self) -> &str {
        &self.service_name
    }

    /// A failed call: an HTTP error, or a problem the proxy recorded.
    pub fn is_error(&self) -> bool {
        status_is_error(self.status) || self.problem.is_some() || self.error.is_some()
    }

    /// Every permission this call accounts for: its own, then implied ones.
    pub fn access(&self) -> impl Iterator<Item = Access> + '_ {
        lenslet_core::trace::trace_access(&self.core)
    }

    /// Each permission with whether it is implied (needed elsewhere).
    pub fn access_entries(&self) -> impl Iterator<Item = (Access, bool, bool)> + '_ {
        self.core
            .access
            .iter()
            .map(|a| (a.key(), false, a.unverified))
            .chain(
                self.core
                    .implied
                    .iter()
                    .map(|a| (a.key(), true, a.unverified)),
            )
    }

    pub fn uses(&self, access: &Access) -> bool {
        self.access().any(|own| own == *access)
    }

    /// Needs a permission `lock` does not hold.
    pub fn not_in_lock(&self, lock: &LockView) -> bool {
        self.access().any(|access| !lock.contains(&access))
    }

    /// The failure identity `lenslet` compares across runs.
    pub fn failure_kind(&self) -> Option<FailureKind> {
        let problem = self.problem.as_ref()?;
        Some(FailureKind {
            service: self.service.clone(),
            operation: self.operation.clone(),
            code: problem.code.clone(),
        })
    }

    /// Semantic call target, falling back to `METHOD /path`.
    pub fn semantic_target(&self) -> String {
        match self.target.as_deref() {
            Some(target) => target.to_string(),
            None => format!(
                "{} {}",
                self.method.to_ascii_uppercase(),
                display_path(self)
            ),
        }
    }

    fn build_haystack(&self) -> String {
        let mut haystack = String::with_capacity(256 + self.path.len());
        for part in [
            self.service.as_str(),
            &self.operation,
            &self.method,
            &self.path,
            &self.host,
            &self.id,
        ] {
            haystack.push_str(part);
            haystack.push('\n');
        }
        let _ = writeln!(haystack, "{} {}", self.status, status_text(self.status));
        if let Some(problem) = self.problem.as_ref() {
            haystack.push_str(&problem.code);
            haystack.push('\n');
            if let Some(message) = problem.message.as_deref() {
                haystack.push_str(message);
                haystack.push('\n');
            }
        }
        for extra in [
            self.region.as_deref(),
            self.target.as_deref(),
            self.error.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            haystack.push_str(extra);
            haystack.push('\n');
        }
        for access in self.access() {
            let _ = writeln!(haystack, "{} {}", access.action, access.resource);
        }
        for message in [&self.request, &self.response] {
            for hdr in &message.headers {
                let _ = writeln!(haystack, "{}: {}", hdr.name, hdr.value);
            }
            if let Some(content_type) = message.content_type.as_deref() {
                haystack.push_str(content_type);
                haystack.push('\n');
            }
            if let Some(text) = message.body.as_ref().and_then(|b| b.text.as_deref()) {
                haystack.push_str(clip_utf8(text, SEARCH_BODY_BYTES));
                haystack.push('\n');
            }
        }
        haystack.to_lowercase()
    }

    fn searchable(&self) -> Cow<'_, str> {
        if self.haystack.is_empty() {
            Cow::Owned(self.build_haystack())
        } else {
            Cow::Borrowed(&self.haystack)
        }
    }
}

/// Wire size of a body (its total bytes, not the captured prefix).
pub fn body_size(body: &Body) -> usize {
    if body.bytes > 0 {
        body.bytes as usize
    } else {
        body.text.as_ref().map_or(0, String::len)
    }
}

/// The exact wire bytes of a body, when the run file holds them (text that
/// was neither truncated nor re-encoded for display).
pub fn wire_bytes(body: &Body) -> Option<Vec<u8>> {
    (!body.binary && !body.truncated && body.encoding.is_none())
        .then(|| body.text.as_deref().unwrap_or("").as_bytes().to_vec())
}

// ---------------------------------------------------------------------------
// Parsing chunks of a file
// ---------------------------------------------------------------------------

/// What a chunk of a run file holds.
#[derive(Default)]
pub struct Parsed {
    pub calls: Vec<Trace>,
    pub header: Option<RunHeader>,
    pub report: Option<Report>,
    pub bad: usize,
}

/// Parse complete lines; `base_offset` is where `bytes` starts in the file.
pub fn parse_records_at(bytes: &[u8], base_offset: u64, source: Option<&Arc<PathBuf>>) -> Parsed {
    let mut parsed = Parsed::default();
    let mut pos = 0usize;
    while pos < bytes.len() {
        let rel_end = bytes[pos..]
            .iter()
            .position(|b| *b == b'\n')
            .unwrap_or(bytes.len() - pos);
        let slice = &bytes[pos..pos + rel_end];
        let line_offset = base_offset + pos as u64;
        pos += rel_end + 1;
        if slice.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        match parse_record(&String::from_utf8_lossy(slice)) {
            Some(Record::Call(mut trace)) => {
                if let Some(path) = source {
                    trace.dehydrate(LineRef {
                        path: path.clone(),
                        offset: line_offset,
                        len: slice.len(),
                    });
                }
                parsed.calls.push(*trace);
            }
            Some(Record::Run(header)) => parsed.header = Some(*header),
            Some(Record::End(report)) => parsed.report = Some(*report),
            None => parsed.bad += 1,
        }
    }
    parsed
}

/// Every call and the run in a run file's text (tests, fixtures).
#[cfg(test)]
pub fn parse_run_text(text: &str) -> (RunInfo, Vec<Trace>) {
    let parsed = parse_records_at(text.as_bytes(), 0, None);
    (
        RunInfo {
            header: parsed.header.unwrap_or_default(),
            report: parsed.report,
        },
        parsed.calls,
    )
}

// ---------------------------------------------------------------------------
// Filters and search
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServiceFacet {
    pub id: String,
    pub label: String,
    pub count: usize,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum StatusFilter {
    #[default]
    All,
    Errors,
    /// Calls that need a permission not in lenslet.lock.
    NotInLock,
}

/// Status AND service AND an explicit id set (a finding's calls).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CallFilter {
    pub status: StatusFilter,
    pub service: Option<String>,
    /// Restrict to these call ids, with a label for the chip.
    pub ids: Option<(String, Arc<HashSet<String>>)>,
}

impl CallFilter {
    pub fn is_active(&self) -> bool {
        self.status != StatusFilter::All || self.service.is_some() || self.ids.is_some()
    }

    pub fn matches(&self, trace: &Trace, lock: &LockView) -> bool {
        let status_ok = match self.status {
            StatusFilter::All => true,
            StatusFilter::Errors => trace.is_error(),
            StatusFilter::NotInLock => trace.not_in_lock(lock),
        };
        status_ok
            && self
                .service
                .as_deref()
                .is_none_or(|svc| trace.service_key() == svc)
            && self
                .ids
                .as_ref()
                .is_none_or(|(_, ids)| ids.contains(&trace.id))
    }
}

pub fn search_tokens(query: &str) -> Vec<String> {
    query
        .split_whitespace()
        .map(|token| token.to_lowercase())
        .collect()
}

pub fn matches_search(trace: &Trace, tokens: &[String]) -> bool {
    if tokens.is_empty() {
        return true;
    }
    let haystack = trace.searchable();
    tokens.iter().all(|token| haystack.contains(token.as_str()))
}

pub fn clip_utf8(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Character-budget truncation with a real ellipsis, for single-line UI cells.
pub fn ellipsize(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    if max_chars == 0 {
        return String::new();
    }
    let mut shortened: String = text.chars().take(max_chars - 1).collect();
    shortened.push('…');
    shortened
}

// ---------------------------------------------------------------------------
// Services
// ---------------------------------------------------------------------------

/// Normalize raw SigV4 / endpoint service names into stable filter identities.
/// Unknown services keep their own name, so every service gets its own chip.
pub fn service_id(service: &str) -> String {
    let lowered = service.trim().to_ascii_lowercase();
    let name = lowered
        .strip_prefix("amazon ")
        .or_else(|| lowered.strip_prefix("aws "))
        .unwrap_or(&lowered);
    match name {
        "" | "aws" => "aws".to_string(),
        "ddb" | "dynamo" => "dynamodb".to_string(),
        "monitoring" => "cloudwatch".to_string(),
        other => other.to_string(),
    }
}

/// Human label for any service, known or not.
pub fn service_label(service: &str) -> String {
    match service_id(service).as_str() {
        "s3" => "S3".to_string(),
        "sqs" => "SQS".to_string(),
        "dynamodb" => "DYNAMODB".to_string(),
        "sns" => "SNS".to_string(),
        "lambda" => "LAMBDA".to_string(),
        "cloudwatch" => "CLOUDWATCH".to_string(),
        "sts" => "STS".to_string(),
        "iam" => "IAM".to_string(),
        "aws" => "AWS".to_string(),
        _ => {
            let display = service.trim();
            let display = if display.is_empty() { "AWS" } else { display };
            clip_utf8(display, 28).to_ascii_uppercase()
        }
    }
}

pub fn operation_label(trace: &Trace) -> &str {
    let operation = trace.operation.trim();
    if !operation.is_empty() {
        operation
    } else if !trace.method.trim().is_empty() {
        trace.method.trim()
    } else {
        "—"
    }
}

pub fn display_path(trace: &Trace) -> &str {
    if trace.path.is_empty() {
        "/"
    } else {
        trace.path.as_str()
    }
}

// ---------------------------------------------------------------------------
// Display helpers
// ---------------------------------------------------------------------------

pub fn status_is_error(status: u16) -> bool {
    status >= 400
}

pub fn status_text(status: u16) -> &'static str {
    match status {
        0 => "unknown",
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        304 => "Not Modified",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        411 => "Length Required",
        412 => "Precondition Failed",
        413 => "Payload Too Large",
        422 => "Unprocessable Entity",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "",
    }
}

pub fn status_label(status: u16) -> String {
    if status == 0 {
        "—".to_string()
    } else {
        status.to_string()
    }
}

pub fn format_latency(ms: u64) -> String {
    if ms == 0 {
        "<1 ms".to_string()
    } else if ms < 1_000 {
        format!("{ms} ms")
    } else if ms < 60_000 {
        trim_zero(format!("{:.2}", ms as f64 / 1_000.0), "s")
    } else {
        let seconds = ms / 1_000;
        format!("{}m {:02}s", seconds / 60, seconds % 60)
    }
}

fn trim_zero(value: String, unit: &str) -> String {
    let value = value
        .strip_suffix(".00")
        .or_else(|| value.strip_suffix('0'))
        .unwrap_or(&value);
    let value = value.strip_suffix('.').unwrap_or(value);
    format!("{value} {unit}")
}

pub fn format_bytes(bytes: usize) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    if bytes < 1024 {
        format!("{bytes} B")
    } else if (bytes as f64) < MB {
        trim_zero(format!("{:.1}", bytes as f64 / KB), "KB")
    } else if (bytes as f64) < GB {
        trim_zero(format!("{:.1}", bytes as f64 / MB), "MB")
    } else {
        trim_zero(format!("{:.1}", bytes as f64 / GB), "GB")
    }
}

/// `2026-11-15T02:24:19.540507869+00:00` -> `02:24:19.540`.
pub fn short_clock(at: &str) -> String {
    let time = at
        .split_once('T')
        .or_else(|| at.split_once(' '))
        .map(|(_, time)| time)
        .unwrap_or(at);
    let time = time.split(['+', '-', 'Z']).next().unwrap_or(time).trim();
    if time.is_empty() {
        "—".to_string()
    } else {
        time.chars().take(12).collect()
    }
}

/// `2026-11-15 02:24:19.540`
pub fn full_time(at: &str) -> String {
    match at.split_once('T').or_else(|| at.split_once(' ')) {
        Some((date, time)) => format!("{date} {}", short_clock(time)),
        None => short_clock(at),
    }
}

/// Indented JSON or XML for display, or `None` when the body is neither (or
/// is a truncated JSON prefix that does not parse).
pub fn pretty_body(text: &str, content_type: &str) -> Option<String> {
    let trimmed = text.trim_start();
    let content_type = content_type.to_ascii_lowercase();
    if content_type.contains("json") || trimmed.starts_with('{') || trimmed.starts_with('[') {
        let value: serde_json::Value = serde_json::from_str(text).ok()?;
        let pretty = serde_json::to_string_pretty(&value).ok()?;
        return (pretty != text).then_some(pretty);
    }
    if content_type.contains("xml") || trimmed.starts_with('<') {
        let pretty = indent_xml(trimmed)?;
        return (pretty != text).then_some(pretty);
    }
    None
}

/// A small, forgiving XML indenter: one element per line, `<a>text</a>` kept
/// together. Not a parser; it only re-flows whitespace between tags.
fn indent_xml(text: &str) -> Option<String> {
    let mut tokens: Vec<&str> = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        if rest.starts_with('<') {
            let end = rest.find('>')? + 1;
            tokens.push(&rest[..end]);
            rest = &rest[end..];
        } else {
            let end = rest.find('<').unwrap_or(rest.len());
            let chunk = rest[..end].trim();
            if !chunk.is_empty() {
                tokens.push(chunk);
            }
            rest = &rest[end..];
        }
    }
    let opening = |token: &str| {
        token.starts_with('<')
            && !token.starts_with("</")
            && !token.starts_with("<?")
            && !token.starts_with("<!")
            && !token.ends_with("/>")
    };
    let mut out = String::new();
    let mut depth = 0usize;
    let mut index = 0;
    while index < tokens.len() {
        let token = tokens[index];
        let indent = "  ".repeat(depth);
        if opening(token) {
            let inline = tokens
                .get(index + 1)
                .is_some_and(|next| !next.starts_with('<'))
                && tokens
                    .get(index + 2)
                    .is_some_and(|close| close.starts_with("</"));
            if inline {
                let _ = writeln!(
                    out,
                    "{indent}{token}{}{}",
                    tokens[index + 1],
                    tokens[index + 2]
                );
                index += 3;
                continue;
            }
            let _ = writeln!(out, "{indent}{token}");
            depth += 1;
        } else if token.starts_with("</") {
            depth = depth.saturating_sub(1);
            let _ = writeln!(out, "{}{token}", "  ".repeat(depth));
        } else {
            let _ = writeln!(out, "{indent}{token}");
        }
        index += 1;
    }
    Some(out.trim_end().to_string())
}

fn posix_single_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// Build an exact, unredacted `curl` command reproducing `trace`'s HTTP request,
/// or `None` when the request carried a binary or truncated body whose complete
/// payload cannot be reconstructed from the trace.
pub fn to_curl(trace: &Trace, endpoint: Option<&str>) -> Option<String> {
    let req = &trace.request;
    if let Some(body) = req.body.as_ref()
        && (body.truncated
            || (body.bytes > 0 && body.text.is_none())
            || body
                .encoding
                .as_deref()
                .is_some_and(|e| e.contains("aws-eventstream")))
    {
        return None;
    }
    let base = endpoint
        .filter(|e| !e.is_empty())
        .map(|e| e.trim_end_matches('/').to_string())
        .or_else(|| (!trace.host.is_empty()).then(|| format!("http://{}", trace.host)))
        .unwrap_or_else(|| "http://127.0.0.1:9297".to_string());
    let path = display_path(trace);
    let url = if path.starts_with('/') {
        format!("{base}{path}")
    } else {
        format!("{base}/{path}")
    };
    let method = if trace.method.trim().is_empty() {
        "GET".to_string()
    } else {
        trace.method.trim().to_ascii_uppercase()
    };
    let mut parts = vec![
        "curl".to_string(),
        "-i".to_string(),
        "-X".to_string(),
        method,
        posix_single_quote(&url),
    ];
    let encoding = req.body.as_ref().and_then(|body| body.encoding.as_deref());
    let decoded_wire = encoding.is_some();
    let compressed_wire = encoding.is_some_and(|enc| {
        let low = enc.to_ascii_lowercase();
        low.contains("gzip") || low.contains("deflate")
    });
    for header in &req.headers {
        let low = header.name.to_ascii_lowercase();
        // Hop-by-hop framing, and wire-encoding headers that no longer match
        // a decoded (`gzip` / `aws-chunked`) `--data-raw` payload.
        if matches!(low.as_str(), "content-length" | "transfer-encoding") {
            continue;
        }
        if decoded_wire
            && (matches!(
                low.as_str(),
                "content-encoding"
                    | "x-amz-decoded-content-length"
                    | "x-amz-trailer"
                    | "x-amz-sdk-checksum-algorithm"
            ) || low.starts_with("x-amz-checksum-")
                || (compressed_wire && low == "content-md5"))
        {
            continue;
        }
        if low == "x-amz-content-sha256"
            && (decoded_wire
                || header
                    .value
                    .trim()
                    .to_ascii_uppercase()
                    .starts_with("STREAMING-"))
        {
            parts.push(format!(
                "-H {}",
                posix_single_quote(&format!("{}: UNSIGNED-PAYLOAD", header.name))
            ));
            continue;
        }
        parts.push(format!(
            "-H {}",
            posix_single_quote(&format!("{}: {}", header.name, header.value))
        ));
    }
    if let Some(text) = req.body.as_ref().and_then(|b| b.text.as_deref())
        && !text.is_empty()
    {
        parts.push(format!("--data-raw {}", posix_single_quote(text)));
    }
    Some(parts.join(" \\\n  "))
}

/// Parse an RFC3339 timestamp into epoch milliseconds, for relative waterfall
/// offsets and idle-gap detection.
pub fn parse_rfc3339_ms(at: &str) -> Option<i64> {
    let (date, rest) = at.trim().split_once(['T', ' '])?;
    let mut d = date.split('-');
    let year: i64 = d.next()?.parse().ok()?;
    let month: i64 = d.next()?.parse().ok()?;
    let day: i64 = d.next()?.parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let rest = rest.strip_suffix('Z').unwrap_or(rest);
    let (clock, tz_minutes) = if let Some((c, off)) = rest.rsplit_once('+') {
        (c, parse_tz_offset(off)?)
    } else if let Some((c, off)) = rest.rsplit_once('-') {
        (c, -parse_tz_offset(off)?)
    } else {
        (rest, 0)
    };
    let mut t = clock.split(':');
    let hour: i64 = t.next()?.parse().ok()?;
    let minute: i64 = t.next()?.parse().ok()?;
    let sec_part = t.next()?;
    let (sec_str, frac_str) = sec_part.split_once('.').unwrap_or((sec_part, ""));
    let second: i64 = sec_str.parse().ok()?;
    let mut millis: i64 = 0;
    for (index, byte) in frac_str.bytes().take(3).enumerate() {
        if !byte.is_ascii_digit() {
            return None;
        }
        let scale = match index {
            0 => 100,
            1 => 10,
            _ => 1,
        };
        millis += i64::from(byte - b'0') * scale;
    }
    let y = year - i64::from(month <= 2);
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = month + if month > 2 { -3 } else { 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let total_seconds = days * 86_400 + hour * 3_600 + minute * 60 + second - tz_minutes * 60;
    Some(total_seconds * 1_000 + millis)
}

fn parse_tz_offset(offset: &str) -> Option<i64> {
    let (h, m) = offset.split_once(':').unwrap_or((offset, "0"));
    let hours: i64 = h.parse().ok()?;
    let mins: i64 = m.parse().ok()?;
    Some(hours * 60 + mins)
}

/// `+142 ms` or `+1.8 s` relative to the start of the run.
pub fn format_offset(delta_ms: i64) -> String {
    let ms = delta_ms.max(0) as u64;
    if ms == 0 {
        "+0 ms".to_string()
    } else {
        format!("+{}", format_latency(ms))
    }
}
/// Smart AWS protocol semantic decoding for the Inspector (`Decoded` mode):
/// - Unwraps DynamoDB AttributeValue wrappers (`{"S": "x", "N": "42"}`) into native JSON;
/// - Decodes `application/x-www-form-urlencoded` AWS Query requests (`Action=...`) with
///   pretty-printed nested JSON values (`MessageBody`, `PolicyDocument`, `Message`, `Detail`);
/// - Pretty-prints `aws-eventstream` newline-delimited event JSON frames;
/// - Unwraps embedded JSON strings (`MessageBody`, `Detail`, `input`, `output`, `Policy`).
///
/// Returns `Some((mode_badge_label, decoded_text, is_form_syntax))` when a richer semantic
/// projection exists beyond standard `pretty_body`.
pub fn decode_aws_semantic(
    text: &str,
    content_type: &str,
    encoding: Option<&str>,
) -> Option<(&'static str, String, bool)> {
    let ct_low = content_type.to_ascii_lowercase();
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }

    // 1. AWS EventStream JSONL (`{"event":"...","data":...}` per line).
    if encoding.is_some_and(|e| e.contains("aws-eventstream")) {
        let mut frames = Vec::new();
        for line in trimmed.lines().filter(|l| !l.trim().is_empty()) {
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(line) {
                frames.push(val);
            }
        }
        if !frames.is_empty() {
            let pretty = serde_json::to_string_pretty(&frames).ok()?;
            return Some(("EventStream", pretty, false));
        }
    }

    // 1b. Smithy RPCv2 CBOR decoded to JSON.
    if encoding.is_some_and(|e| e.contains("cbor"))
        && let Ok(val) = serde_json::from_str::<serde_json::Value>(trimmed)
        && let Ok(pretty) = serde_json::to_string_pretty(&val)
    {
        return Some(("Smithy CBOR", pretty, false));
    }

    // 2. AWS Query `x-www-form-urlencoded` (`Action=SendMessage&...`).
    if (ct_low.contains("x-www-form-urlencoded")
        || (!trimmed.starts_with('{')
            && !trimmed.starts_with('<')
            && trimmed.starts_with("Action=")))
        && let Some(decoded_form) = format_query_form(trimmed)
    {
        return Some(("Query Params", decoded_form, true));
    }

    // 3. JSON payloads: DynamoDB AttributeValue unwrapping or embedded JSON string unwrapping.
    if (ct_low.contains("json") || trimmed.starts_with('{'))
        && let Ok(val) = serde_json::from_str::<serde_json::Value>(trimmed)
    {
        let mut ddb_hits = 0usize;
        let unwrapped = unwrap_ddb_and_embedded(&val, false, &mut ddb_hits);
        if ddb_hits > 0 {
            let out = serde_json::to_string_pretty(&unwrapped).ok()?;
            return Some(("DynamoDB Unwrapped", out, false));
        }
        let mut embedded_hits = 0usize;
        let emb = unwrap_embedded_json_strings(&val, &mut embedded_hits);
        if embedded_hits > 0 {
            let out = serde_json::to_string_pretty(&emb).ok()?;
            return Some(("Decoded JSON", out, false));
        }
    }

    None
}

fn format_query_form(raw: &str) -> Option<String> {
    let mut pairs = Vec::new();
    for part in raw.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = part.split_once('=').unwrap_or((part, ""));
        let key = percent_decode_form(k);
        let val = percent_decode_form(v);
        pairs.push((key, val));
    }
    if pairs.is_empty() {
        return None;
    }
    let mut out = String::new();
    for (k, v) in pairs {
        let v_trim = v.trim();
        if ((v_trim.starts_with('{') && v_trim.ends_with('}'))
            || (v_trim.starts_with('[') && v_trim.ends_with(']')))
            && let Ok(json_val) = serde_json::from_str::<serde_json::Value>(v_trim)
            && let Ok(pretty) = serde_json::to_string_pretty(&json_val)
        {
            let _ = writeln!(out, "{k} =");
            for line in pretty.lines() {
                let _ = writeln!(out, "  {line}");
            }
            continue;
        }
        let _ = writeln!(out, "{k} = {v}");
    }
    Some(out.trim_end().to_string())
}

fn percent_decode_form(input: &str) -> String {
    fn hex(b: u8) -> Option<u8> {
        (b as char).to_digit(16).map(|d| d as u8)
    }
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (hex(bytes[i + 1]), hex(bytes[i + 2]))
        {
            out.push((hi << 4) | lo);
            i += 3;
            continue;
        }
        out.push(if bytes[i] == b'+' { b' ' } else { bytes[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn unwrap_ddb_and_embedded(
    val: &serde_json::Value,
    in_ddb_container: bool,
    ddb_hits: &mut usize,
) -> serde_json::Value {
    const DDB_CONTAINERS: &[&str] = &[
        "Item",
        "Items",
        "Key",
        "Keys",
        "Attributes",
        "ExpressionAttributeValues",
        "LastEvaluatedKey",
        "ExclusiveStartKey",
        "Responses",
        "RequestItems",
        "TransactItems",
    ];

    match val {
        serde_json::Value::Object(map) => {
            if in_ddb_container
                && map.len() == 1
                && let Some((tag, inner)) = map.iter().next()
            {
                match (tag.as_str(), inner) {
                    ("S" | "B", serde_json::Value::String(s)) => {
                        *ddb_hits += 1;
                        return serde_json::Value::String(s.clone());
                    }
                    ("N", serde_json::Value::String(n)) => {
                        *ddb_hits += 1;
                        if let Ok(i) = n.parse::<i64>() {
                            return serde_json::Value::Number(i.into());
                        }
                        if let Ok(f) = n.parse::<f64>()
                            && let Some(num) = serde_json::Number::from_f64(f)
                        {
                            return serde_json::Value::Number(num);
                        }
                        return serde_json::Value::String(n.clone());
                    }
                    ("BOOL", serde_json::Value::Bool(b)) => {
                        *ddb_hits += 1;
                        return serde_json::Value::Bool(*b);
                    }
                    ("NULL", serde_json::Value::Bool(true)) => {
                        *ddb_hits += 1;
                        return serde_json::Value::Null;
                    }
                    ("SS" | "BS", serde_json::Value::Array(arr)) => {
                        *ddb_hits += 1;
                        return serde_json::Value::Array(arr.clone());
                    }
                    ("NS", serde_json::Value::Array(arr)) => {
                        *ddb_hits += 1;
                        let nums = arr
                            .iter()
                            .map(|v| {
                                v.as_str()
                                    .and_then(|s| s.parse::<f64>().ok())
                                    .and_then(serde_json::Number::from_f64)
                                    .map_or_else(|| v.clone(), serde_json::Value::Number)
                            })
                            .collect();
                        return serde_json::Value::Array(nums);
                    }
                    ("L", serde_json::Value::Array(arr)) => {
                        *ddb_hits += 1;
                        return serde_json::Value::Array(
                            arr.iter()
                                .map(|item| unwrap_ddb_and_embedded(item, true, ddb_hits))
                                .collect(),
                        );
                    }
                    ("M", serde_json::Value::Object(sub)) => {
                        *ddb_hits += 1;
                        let mut out = serde_json::Map::new();
                        for (k, v) in sub {
                            out.insert(k.clone(), unwrap_ddb_and_embedded(v, true, ddb_hits));
                        }
                        return serde_json::Value::Object(out);
                    }
                    _ => {}
                }
            }
            let mut out = serde_json::Map::new();
            for (k, v) in map {
                let child_in_ddb = in_ddb_container || DDB_CONTAINERS.contains(&k.as_str());
                out.insert(
                    k.clone(),
                    unwrap_ddb_and_embedded(v, child_in_ddb, ddb_hits),
                );
            }
            serde_json::Value::Object(out)
        }
        serde_json::Value::Array(arr) => serde_json::Value::Array(
            arr.iter()
                .map(|v| unwrap_ddb_and_embedded(v, in_ddb_container, ddb_hits))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn unwrap_embedded_json_strings(val: &serde_json::Value, hits: &mut usize) -> serde_json::Value {
    const JSON_STRING_FIELDS: &[&str] = &[
        "MessageBody",
        "Body",
        "Message",
        "Detail",
        "Policy",
        "PolicyDocument",
        "AssumeRolePolicyDocument",
        "input",
        "output",
    ];
    match val {
        serde_json::Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (k, v) in map {
                if JSON_STRING_FIELDS.contains(&k.as_str())
                    && let serde_json::Value::String(s) = v
                {
                    let st = s.trim();
                    if ((st.starts_with('{') && st.ends_with('}'))
                        || (st.starts_with('[') && st.ends_with(']')))
                        && let Ok(parsed) = serde_json::from_str::<serde_json::Value>(st)
                    {
                        *hits += 1;
                        out.insert(k.clone(), unwrap_embedded_json_strings(&parsed, hits));
                        continue;
                    }
                }
                out.insert(k.clone(), unwrap_embedded_json_strings(v, hits));
            }
            serde_json::Value::Object(out)
        }
        serde_json::Value::Array(arr) => serde_json::Value::Array(
            arr.iter()
                .map(|v| unwrap_embedded_json_strings(v, hits))
                .collect(),
        ),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        store::{LockState, access_rows, not_in_lock_count},
        testdata::{LOCK_V4, LOG_V4, RUN_V4, scratch},
    };

    fn fixture_lock() -> LockView {
        LockView {
            path: Some("lenslet.lock".into()),
            lock: Some(Arc::new(lenslet_core::lock::parse(LOCK_V4))),
        }
    }

    #[test]
    fn reads_a_format_4_run_with_the_core_types() {
        let (run, calls) = parse_run_text(RUN_V4);
        assert_eq!(run.header.version, RUN_FORMAT);
        assert_eq!(version_error(run.header.version), None);
        assert_eq!(emulator_name(&run.header).as_deref(), Some("moto"));
        assert_eq!(run.command_line(), "python demo.py");
        assert_eq!(calls.len(), 15);
        let report = run.report.as_ref().unwrap();
        assert_eq!(report.exit_code, Some(1));
        assert_eq!(run.failures().len(), 2);
        assert_eq!(run.regressions().unwrap().new_failures.len(), 1);
        let failed = calls.iter().find(|c| c.status == 404).unwrap();
        assert!(failed.is_error());
        assert_eq!(failed.problem.as_ref().unwrap().code, "NoSuchKey");
        assert_eq!(failed.service_name(), "S3");
        assert!(failed.semantic_target().contains("shop-assets"));
        // KMS implied by SSE: own + implied permissions.
        let put = calls.iter().find(|c| c.operation == "PutObject").unwrap();
        let needs: Vec<(String, bool)> = put
            .access_entries()
            .map(|(a, implied, _)| (a.action, implied))
            .collect();
        assert_eq!(
            needs,
            [
                ("s3:PutObject".to_string(), false),
                ("kms:GenerateDataKey".to_string(), true)
            ]
        );
    }

    #[test]
    fn an_older_run_file_is_refused_with_a_message() {
        let error = version_error(3).unwrap();
        assert!(error.en.contains("re-run your tests"));
        assert!(error.zh.contains("重新运行"));
        assert!(version_error(5).is_some());
    }

    /// One source of truth: with the fixture's lock exactly one permission
    /// (and one call) is not in it; with no lock file, all of them are.
    #[test]
    fn in_lock_state_agrees_for_rows_calls_and_counts() {
        let (run, calls) = parse_run_text(RUN_V4);
        let used = report_access(run.report.as_ref().unwrap());
        assert_eq!(used.len(), 17);

        let lock = fixture_lock();
        let rows = access_rows(&used, &lock);
        assert_eq!(not_in_lock_count(&rows), 1);
        assert_eq!(rows[0].access().action, "dynamodb:DeleteItem");
        assert_eq!(rows[0].state, LockState::NotInLock);
        let calls_not_in_lock: Vec<&str> = calls
            .iter()
            .filter(|c| c.not_in_lock(&lock))
            .map(|c| c.operation.as_str())
            .collect();
        assert_eq!(calls_not_in_lock, ["DeleteItem"]);
        assert!(rows.iter().any(|r| r.test), "# test lines are read");

        let none = LockView::default();
        let rows = access_rows(&used, &none);
        assert_eq!(not_in_lock_count(&rows), 17);
        assert!(
            rows.iter()
                .all(|r| r.state == LockState::NotInLock && !r.in_lock())
        );
        let with_access = calls.iter().filter(|c| c.access().next().is_some()).count();
        assert_eq!(
            calls.iter().filter(|c| c.not_in_lock(&none)).count(),
            with_access
        );
    }

    #[test]
    fn finds_the_run_log_next_to_the_run_file() {
        let dir = scratch("model-log");
        let run_path = dir.join("20261115-094842-6721.jsonl");
        std::fs::write(&run_path, RUN_V4).unwrap();
        std::fs::write(dir.join("20261115-094842-6721.log"), LOG_V4).unwrap();
        let (run, _) = parse_run_text(RUN_V4);
        let log = log_path(&run, &run_path);
        assert_eq!(log, dir.join("20261115-094842-6721.log"));
        let (_, text) = crate::feed::read_log(&log, None).unwrap();
        assert!(text.unwrap().contains("NoSuchKey"));
        // No `log` in the header: `<run>.log`.
        let mut bare = run.clone();
        bare.header.log = None;
        assert_eq!(log_path(&bare, &run_path), log);
    }

    #[test]
    fn copies_a_call_as_curl() {
        let (_, calls) = parse_run_text(RUN_V4);
        let put = calls.iter().find(|c| c.operation == "PutItem").unwrap();
        let curl = to_curl(put, Some("http://127.0.0.1:1/")).unwrap();
        assert!(curl.starts_with("curl") && curl.contains("POST"));
        assert!(curl.contains("'http://127.0.0.1:1/'"));
        assert!(curl.contains("x-amz-target: DynamoDB_20120810.PutItem"));
        assert!(!curl.contains("content-length"));
    }
}
