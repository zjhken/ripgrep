use std::cmp;
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::io::{self, BufRead};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use clap;
use encoding_rs::Encoding;
use grep::{Grep, GrepBuilder};
use grep_printer;
use grep_regex;
use grep_searcher;
use log;
use num_cpus;
use regex;
use same_file;
use termcolor;

use app;
use atty;
use ignore::overrides::{Override, OverrideBuilder};
use ignore::types::{FileTypeDef, Types, TypesBuilder};
use ignore;
use printer::{ColorSpecs, Printer};
use unescape::{escape, unescape};
use worker::{Worker, WorkerBuilder};

use config;
use logger::Logger;
use Result;

/// `Args` are transformed/normalized from `ArgMatches`.
#[derive(Debug)]
pub struct Args {
    paths: Vec<PathBuf>,
    after_context: usize,
    before_context: usize,
    byte_offset: bool,
    can_match: bool,
    color_choice: termcolor::ColorChoice,
    colors: ColorSpecs,
    column: bool,
    context_separator: Vec<u8>,
    count: bool,
    count_matches: bool,
    encoding: Option<&'static Encoding>,
    files_with_matches: bool,
    files_without_matches: bool,
    eol: u8,
    files: bool,
    follow: bool,
    glob_overrides: Override,
    grep: Grep,
    heading: bool,
    hidden: bool,
    ignore_files: Vec<PathBuf>,
    invert_match: bool,
    line_number: bool,
    line_per_match: bool,
    matcher: grep_regex::RegexMatcher,
    max_columns: Option<usize>,
    max_count: Option<u64>,
    max_depth: Option<usize>,
    max_filesize: Option<u64>,
    mmap: bool,
    multiline: bool,
    no_ignore: bool,
    no_ignore_global: bool,
    no_ignore_messages: bool,
    no_ignore_parent: bool,
    no_ignore_vcs: bool,
    no_messages: bool,
    null: bool,
    only_matching: bool,
    path_separator: Option<u8>,
    quiet: bool,
    quiet_matched: QuietMatched,
    replace: Option<Vec<u8>>,
    sort_files: bool,
    stdout_handle: Option<same_file::Handle>,
    text: bool,
    threads: usize,
    type_list: bool,
    types: Types,
    with_filename: bool,
    search_zip_files: bool,
    preprocessor: Option<PathBuf>,
    stats: bool,
    user_color_specs: Vec<grep_printer::UserColorSpec>,
}

impl Args {
    /// Parse the command line arguments for this process.
    ///
    /// If a CLI usage error occurred, then exit the process and print a usage
    /// or error message. Similarly, if the user requested the version of
    /// ripgrep, then print the version and exit.
    ///
    /// Also, initialize a global logger.
    pub fn parse() -> Result<Args> {
        // We parse the args given on CLI. This does not include args from
        // the config. We use the CLI args as an initial configuration while
        // trying to parse config files. If a config file exists and has
        // arguments, then we re-parse argv, otherwise we just use the matches
        // we have here.
        let early_matches = ArgMatches(app::app().get_matches());

        if let Err(err) = Logger::init() {
            errored!("failed to initialize logger: {}", err);
        }
        if early_matches.is_present("debug") {
            log::set_max_level(log::LevelFilter::Debug);
        } else {
            log::set_max_level(log::LevelFilter::Warn);
        }

        let matches = Args::matches(early_matches);
        // The logging level may have changed if we brought in additional
        // arguments from a configuration file, so recheck it and set the log
        // level as appropriate.
        if matches.is_present("debug") {
            log::set_max_level(log::LevelFilter::Debug);
        } else {
            log::set_max_level(log::LevelFilter::Warn);
        }
        matches.to_args()
    }

    /// Run clap and return the matches. If clap determines a problem with the
    /// user provided arguments (or if --help or --version are given), then an
    /// error/usage/version will be printed and the process will exit.
    ///
    /// If there are no additional arguments from the environment (e.g., a
    /// config file), then the given matches are returned as is.
    fn matches(early_matches: ArgMatches<'static>) -> ArgMatches<'static> {
        // If the end user says no config, then respect it.
        if early_matches.is_present("no-config") {
            debug!("not reading config files because --no-config is present");
            return early_matches;
        }
        // If the user wants ripgrep to use a config file, then parse args
        // from that first.
        let mut args = config::args(early_matches.is_present("no-messages"));
        if args.is_empty() {
            return early_matches;
        }
        let mut cliargs = env::args_os();
        if let Some(bin) = cliargs.next() {
            args.insert(0, bin);
        }
        args.extend(cliargs);
        debug!("final argv: {:?}", args);
        ArgMatches(app::app().get_matches_from(args))
    }

    /// Returns true if ripgrep should print the files it will search and exit
    /// (but not do any actual searching).
    pub fn files(&self) -> bool {
        self.files
    }

    /// Create a new line based matcher. The matcher returned can be used
    /// across multiple threads simultaneously. This matcher only supports
    /// basic searching of regular expressions in a single buffer.
    ///
    /// The pattern and other flags are taken from the command line.
    pub fn grep(&self) -> Grep {
        self.grep.clone()
    }

    /// Whether ripgrep should be quiet or not.
    pub fn quiet(&self) -> bool {
        self.quiet
    }

    /// Returns a thread safe boolean for determining whether to quit a search
    /// early when quiet mode is enabled.
    ///
    /// If quiet mode is disabled, then QuietMatched.has_match always returns
    /// false.
    pub fn quiet_matched(&self) -> QuietMatched {
        self.quiet_matched.clone()
    }

    /// Create a new printer of individual search results that writes to the
    /// writer given.
    pub fn printer<W: termcolor::WriteColor>(&self, wtr: W) -> Printer<W> {
        let mut p = Printer::new(wtr)
            .colors(self.colors.clone())
            .column(self.column)
            .context_separator(self.context_separator.clone())
            .eol(self.eol)
            .heading(self.heading)
            .line_per_match(self.line_per_match)
            .null(self.null)
            .only_matching(self.only_matching)
            .path_separator(self.path_separator)
            .with_filename(self.with_filename)
            .max_columns(self.max_columns);
        if let Some(ref rep) = self.replace {
            p = p.replace(rep.clone());
        }
        p
    }

    /// Create a new printer of individual search results that writes to the
    /// writer given.
    pub fn printer2<W: termcolor::WriteColor>(
        &self,
        wtr: W,
    ) -> grep_printer::Standard<W> {
        grep_printer::StandardBuilder::new()
            .user_color_specs(&self.user_color_specs)
            .heading(self.heading)
            .only_matching(self.only_matching)
            .line_per_match(self.line_per_match)
            .replacement(self.replace.clone())
            .max_columns(self.max_columns.map(|n| n as u64))
            .column(self.column)
            .byte_offset(self.byte_offset)
            .separator_context(Some(self.context_separator.clone()))
            .separator_path(self.path_separator)
            .path_terminator(if self.null { Some(b'\x00') } else { None })
            .build(wtr)
    }

    pub fn searcher2(&self) -> Result<grep_searcher::Searcher> {
        let mut builder = grep_searcher::SearcherBuilder::new();
        builder
            .invert_match(self.invert_match)
            .line_number(self.line_number)
            .multi_line(self.multiline)
            .after_context(self.after_context)
            .before_context(self.before_context)
            .memory_map(grep_searcher::MmapChoice::never());
        if !self.text {
            builder.binary_detection(
                grep_searcher::BinaryDetection::quit(b'\x00')
            );
        }
        Ok(builder.build()?)
    }

    pub fn matcher2(&self) -> grep_regex::RegexMatcher {
        self.matcher.clone()
    }

    /// Retrieve the configured file separator.
    pub fn file_separator(&self) -> Option<Vec<u8>> {
        let contextless =
            self.count
            || self.count_matches
            || self.files_with_matches
            || self.files_without_matches;
        let use_heading_sep = self.heading && !contextless;

        if use_heading_sep {
            Some(b"".to_vec())
        } else if !contextless
            && (self.before_context > 0 || self.after_context > 0) {
            Some(self.context_separator.clone())
        } else {
            None
        }
    }

    /// Returns true if the given arguments are known to never produce a match.
    pub fn never_match(&self) -> bool {
        !self.can_match || self.max_count == Some(0)
    }

    /// Returns whether ripgrep should track stats for this run
    pub fn stats(&self) -> bool {
        self.stats
    }

    /// Create a new writer for single-threaded searching with color support.
    pub fn stdout(&self) -> Box<termcolor::WriteColor> {
        if atty::is(atty::Stream::Stdout) {
            Box::new(termcolor::StandardStream::stdout(self.color_choice))
        } else {
            Box::new(
                termcolor::BufferedStandardStream::stdout(self.color_choice))
        }
    }

    /// Returns a handle to stdout for filtering search.
    ///
    /// A handle is returned if and only if ripgrep's stdout is being
    /// redirected to a file. The handle returned corresponds to that file.
    ///
    /// This can be used to ensure that we do not attempt to search a file
    /// that ripgrep is writing to.
    pub fn stdout_handle(&self) -> Option<&same_file::Handle> {
        self.stdout_handle.as_ref()
    }

    /// Create a new buffer writer for multi-threaded searching with color
    /// support.
    pub fn buffer_writer(&self) -> termcolor::BufferWriter {
        let mut wtr = termcolor::BufferWriter::stdout(self.color_choice);
        wtr.separator(self.file_separator());
        wtr
    }

    /// Return the paths that should be searched.
    pub fn paths(&self) -> &[PathBuf] {
        &self.paths
    }

    /// Returns true if there is exactly one file path given to search.
    pub fn is_one_path(&self) -> bool {
        self.paths.len() == 1
        && (self.paths[0] == Path::new("-") || path_is_file(&self.paths[0]))
    }

    /// Create a worker whose configuration is taken from the
    /// command line.
    pub fn worker(&self) -> Worker {
        WorkerBuilder::new(self.grep())
            .after_context(self.after_context)
            .before_context(self.before_context)
            .byte_offset(self.byte_offset)
            .count(self.count)
            .count_matches(self.count_matches)
            .encoding(self.encoding)
            .files_with_matches(self.files_with_matches)
            .files_without_matches(self.files_without_matches)
            .eol(self.eol)
            .line_number(self.line_number)
            .invert_match(self.invert_match)
            .max_count(self.max_count)
            .mmap(self.mmap)
            .no_messages(self.no_messages)
            .quiet(self.quiet)
            .text(self.text)
            .search_zip_files(self.search_zip_files)
            .preprocessor(self.preprocessor.clone())
            .build()
    }

    /// Returns the number of worker search threads that should be used.
    pub fn threads(&self) -> usize {
        self.threads
    }

    /// Returns a list of type definitions currently loaded.
    pub fn type_defs(&self) -> &[FileTypeDef] {
        self.types.definitions()
    }

    /// Returns true if ripgrep should print the type definitions currently
    /// loaded and then exit.
    pub fn type_list(&self) -> bool {
        self.type_list
    }

    /// Returns true if error messages should be suppressed.
    pub fn no_messages(&self) -> bool {
        self.no_messages
    }

    /// Returns true if error messages associated with parsing .ignore or
    /// .gitignore files should be suppressed.
    pub fn no_ignore_messages(&self) -> bool {
        self.no_ignore_messages
    }

    /// Create a new recursive directory iterator over the paths in argv.
    pub fn walker(&self) -> ignore::Walk {
        self.walker_builder().build()
    }

    /// Create a new parallel recursive directory iterator over the paths
    /// in argv.
    pub fn walker_parallel(&self) -> ignore::WalkParallel {
        self.walker_builder().build_parallel()
    }

    fn walker_builder(&self) -> ignore::WalkBuilder {
        let paths = self.paths();
        let mut wd = ignore::WalkBuilder::new(&paths[0]);
        for path in &paths[1..] {
            wd.add(path);
        }
        for path in &self.ignore_files {
            if let Some(err) = wd.add_ignore(path) {
                if !self.no_messages && !self.no_ignore_messages {
                    eprintln!("{}", err);
                }
            }
        }

        wd.follow_links(self.follow);
        wd.hidden(!self.hidden);
        wd.max_depth(self.max_depth);
        wd.max_filesize(self.max_filesize);
        wd.overrides(self.glob_overrides.clone());
        wd.types(self.types.clone());
        wd.git_global(
            !self.no_ignore && !self.no_ignore_vcs && !self.no_ignore_global
        );
        wd.git_ignore(!self.no_ignore && !self.no_ignore_vcs);
        wd.git_exclude(!self.no_ignore && !self.no_ignore_vcs);
        wd.ignore(!self.no_ignore);
        if !self.no_ignore {
            wd.add_custom_ignore_filename(".rgignore");
        }
        wd.parents(!self.no_ignore_parent);
        wd.threads(self.threads());
        if self.sort_files {
            wd.sort_by_file_name(|a, b| a.cmp(b));
        }
        wd
    }
}

/// `ArgMatches` wraps `clap::ArgMatches` and provides semantic meaning to
/// several options/flags.
struct ArgMatches<'a>(clap::ArgMatches<'a>);

impl<'a> ArgMatches<'a> {
    /// Convert the result of parsing CLI arguments into ripgrep's
    /// configuration.
    fn to_args(&self) -> Result<Args> {
        let paths = self.paths();
        let line_number = self.line_number(&paths);
        let mmap = self.mmap(&paths)?;
        let with_filename = self.with_filename(&paths);
        let (before_context, after_context) = self.contexts()?;
        let (count, count_matches) = self.counts();
        let quiet = self.is_present("quiet");
        let (grep, can_match) = self.grep()?;
        let (matcher2, _) = self.matcher2()?;
        let args = Args {
            paths: paths,
            after_context: after_context,
            before_context: before_context,
            byte_offset: self.is_present("byte-offset"),
            can_match: can_match,
            color_choice: self.color_choice(),
            colors: self.color_specs()?,
            column: self.column(),
            context_separator: self.context_separator(),
            count: count,
            count_matches: count_matches,
            encoding: self.encoding()?,
            files_with_matches: self.is_present("files-with-matches"),
            files_without_matches: self.is_present("files-without-match"),
            eol: b'\n',
            files: self.is_present("files"),
            follow: self.is_present("follow"),
            glob_overrides: self.overrides()?,
            grep: grep,
            heading: self.heading(),
            hidden: self.hidden(),
            ignore_files: self.ignore_files(),
            invert_match: self.is_present("invert-match"),
            line_number: line_number,
            line_per_match: self.is_present("vimgrep"),
            matcher: matcher2,
            max_columns: self.usize_of_nonzero("max-columns")?,
            max_count: self.usize_of("max-count")?.map(|n| n as u64),
            max_depth: self.usize_of("max-depth")?,
            max_filesize: self.max_filesize()?,
            mmap: mmap,
            multiline: self.is_present("multiline"),
            no_ignore: self.no_ignore(),
            no_ignore_global: self.no_ignore_global(),
            no_ignore_messages: self.is_present("no-ignore-messages"),
            no_ignore_parent: self.no_ignore_parent(),
            no_ignore_vcs: self.no_ignore_vcs(),
            no_messages: self.is_present("no-messages"),
            null: self.is_present("null"),
            only_matching: self.is_present("only-matching"),
            path_separator: self.path_separator()?,
            quiet: quiet,
            quiet_matched: QuietMatched::new(quiet),
            replace: self.replace(),
            sort_files: self.is_present("sort-files"),
            stdout_handle: self.stdout_handle(),
            text: self.text(),
            threads: self.threads()?,
            type_list: self.is_present("type-list"),
            types: self.types()?,
            with_filename: with_filename,
            search_zip_files: self.is_present("search-zip"),
            preprocessor: self.preprocessor(),
            stats: self.stats(),
            user_color_specs: self.user_color_specs()?,
        };
        if args.mmap {
            debug!("will try to use memory maps");
        }
        Ok(args)
    }

    /// Return all file paths that ripgrep should search.
    fn paths(&self) -> Vec<PathBuf> {
        let mut paths: Vec<PathBuf> = match self.values_of_os("path") {
            None => vec![],
            Some(vals) => vals.map(|p| Path::new(p).to_path_buf()).collect(),
        };
        // If --file, --files or --regexp is given, then the first path is
        // always in `pattern`.
        if self.is_present("file")
            || self.is_present("files")
            || self.is_present("regexp") {
            if let Some(path) = self.value_of_os("pattern") {
                paths.insert(0, Path::new(path).to_path_buf());
            }
        }
        if paths.is_empty() {
            paths.push(self.default_path());
        }
        paths
    }

    /// Return the default path that ripgrep should search.
    fn default_path(&self) -> PathBuf {
        let file_is_stdin =
            self.values_of_os("file").map_or(false, |mut files| {
                files.any(|f| f == "-")
            });
        let search_cwd = atty::is(atty::Stream::Stdin)
            || !stdin_is_readable()
            || (self.is_present("file") && file_is_stdin)
            || self.is_present("files")
            || self.is_present("type-list");
        if search_cwd {
            Path::new("./").to_path_buf()
        } else {
            Path::new("-").to_path_buf()
        }
    }

    /// Return all of the ignore files given on the command line.
    fn ignore_files(&self) -> Vec<PathBuf> {
        match self.values_of_os("ignore-file") {
            None => return vec![],
            Some(vals) => vals.map(|p| Path::new(p).to_path_buf()).collect(),
        }
    }

    /// Get a sequence of all available patterns from the command line.
    /// This includes reading the -e/--regexp and -f/--file flags.
    ///
    /// Note that if -F/--fixed-strings is set, then all patterns will be
    /// escaped. Similarly, if -w/--word-regexp is set, then all patterns
    /// are surrounded by `\b`, and if -x/--line-regexp is set, then all
    /// patterns are surrounded by `^...$`. Finally, if --passthru is set,
    /// the pattern `^` is added to the end (to ensure that it works as
    /// expected with multiple -e/-f patterns).
    ///
    /// If any pattern is invalid UTF-8, then an error is returned.
    fn patterns(&self) -> Result<Vec<String>> {
        if self.is_present("files") || self.is_present("type-list") {
            return Ok(vec![self.empty_pattern()]);
        }
        let mut pats = vec![];
        match self.values_of_os("regexp") {
            None => {
                if self.values_of_os("file").is_none() {
                    if let Some(os_pat) = self.value_of_os("pattern") {
                        pats.push(self.os_str_pattern(os_pat)?);
                    }
                }
            }
            Some(os_pats) => {
                for os_pat in os_pats {
                    pats.push(self.os_str_pattern(os_pat)?);
                }
            }
        }
        if let Some(files) = self.values_of_os("file") {
            for file in files {
                if file == "-" {
                    let stdin = io::stdin();
                    for line in stdin.lock().lines() {
                        pats.push(self.str_pattern(&line?));
                    }
                } else {
                    let f = fs::File::open(file)?;
                    for line in io::BufReader::new(f).lines() {
                        pats.push(self.str_pattern(&line?));
                    }
                }
            }
        }
        // It's important that this be at the end; otherwise it would always
        // match first, and we wouldn't get colours in the output
        if self.is_present("passthru") && !self.is_present("count") {
            pats.push("^".to_string())
        }
        Ok(pats)
    }

    /// Converts an OsStr pattern to a String pattern, including line/word
    /// boundaries or escapes if applicable.
    ///
    /// If the pattern is not valid UTF-8, then an error is returned.
    fn os_str_pattern(&self, pat: &OsStr) -> Result<String> {
        let s = pattern_to_str(pat)?;
        Ok(self.str_pattern(s))
    }

    /// Converts a &str pattern to a String pattern, including line/word
    /// boundaries or escapes if applicable.
    fn str_pattern(&self, pat: &str) -> String {
        let litpat = self.literal_pattern(pat.to_string());
        let s = self.line_pattern(self.word_pattern(litpat));

        if s.is_empty() {
            self.empty_pattern()
        } else {
            s
        }
    }

    /// Returns the given pattern as a literal pattern if the
    /// -F/--fixed-strings flag is set. Otherwise, the pattern is returned
    /// unchanged.
    fn literal_pattern(&self, pat: String) -> String {
        if self.is_present("fixed-strings") {
            regex::escape(&pat)
        } else {
            pat
        }
    }

    /// Returns the given pattern as a word pattern if the -w/--word-regexp
    /// flag is set. Otherwise, the pattern is returned unchanged.
    fn word_pattern(&self, pat: String) -> String {
        if self.is_present("word-regexp") {
            pat
            // format!(r"\b(?:{})\b", pat)
        } else {
            pat
        }
    }

    /// Returns the given pattern as a line pattern if the -x/--line-regexp
    /// flag is set. Otherwise, the pattern is returned unchanged.
    fn line_pattern(&self, pat: String) -> String {
        if self.is_present("line-regexp") {
            format!(r"^(?:{})$", pat)
        } else {
            pat
        }
    }

    /// Empty pattern returns a pattern that is guaranteed to produce an empty
    /// regular expression that is valid in any position.
    fn empty_pattern(&self) -> String {
        // This would normally just be an empty string, which works on its
        // own, but if the patterns are joined in a set of alternations, then
        // you wind up with `foo|`, which is invalid.
        self.word_pattern("(?:z{0})*".to_string())
    }

    /// Returns true if and only if file names containing each match should
    /// be emitted.
    ///
    /// `paths` should be a slice of all top-level file paths that ripgrep
    /// will need to search.
    fn with_filename(&self, paths: &[PathBuf]) -> bool {
        if self.is_present("no-filename") {
            false
        } else {
            self.is_present("with-filename")
            || self.is_present("vimgrep")
            || paths.len() > 1
            || paths.get(0).map_or(false, |p| path_is_dir(p))
        }
    }

    /// Returns a handle to stdout for filtering search.
    ///
    /// A handle is returned if and only if ripgrep's stdout is being
    /// redirected to a file. The handle returned corresponds to that file.
    ///
    /// This can be used to ensure that we do not attempt to search a file
    /// that ripgrep is writing to.
    fn stdout_handle(&self) -> Option<same_file::Handle> {
        let h = match same_file::Handle::stdout() {
            Err(_) => return None,
            Ok(h) => h,
        };
        let md = match h.as_file().metadata() {
            Err(_) => return None,
            Ok(md) => md,
        };
        if !md.is_file() {
            return None;
        }
        Some(h)
    }

    /// Returns true if and only if memory map searching should be tried.
    ///
    /// `paths` should be a slice of all top-level file paths that ripgrep
    /// will need to search.
    fn mmap(&self, paths: &[PathBuf]) -> Result<bool> {
        let (before, after) = self.contexts()?;
        let enc = self.encoding()?;
        Ok(if before > 0 || after > 0 || self.is_present("no-mmap") {
            false
        } else if self.is_present("mmap") {
            true
        } else if cfg!(target_os = "macos") {
            // On Mac, memory maps appear to suck. Neat.
            false
        } else if enc.is_some() {
            // There's no practical way to transcode a memory map that isn't
            // isomorphic to searching over io::Read.
            false
        } else {
            // If we're only searching a few paths and all of them are
            // files, then memory maps are probably faster.
            paths.len() <= 10 && paths.iter().all(|p| path_is_file(p))
        })
    }

    /// Returns true if and only if line numbers should be shown.
    fn line_number(&self, paths: &[PathBuf]) -> bool {
        if self.is_present("no-line-number") || self.is_present("count") {
            false
        } else {
            let only_stdin = paths == [Path::new("-")];
            (atty::is(atty::Stream::Stdout) && !only_stdin)
            || self.is_present("line-number")
            || self.is_present("column")
            || self.is_present("pretty")
            || self.is_present("vimgrep")
        }
    }

    /// Returns true if and only if column numbers should be shown.
    fn column(&self) -> bool {
        if self.is_present("no-column") {
            return false;
        }
        self.is_present("column") || self.is_present("vimgrep")
    }

    /// Returns true if and only if matches should be grouped with file name
    /// headings.
    fn heading(&self) -> bool {
        if self.is_present("no-heading") || self.is_present("vimgrep") {
            false
        } else {
            atty::is(atty::Stream::Stdout)
            || self.is_present("heading")
            || self.is_present("pretty")
        }
    }

    /// Returns the replacement string as UTF-8 bytes if it exists.
    fn replace(&self) -> Option<Vec<u8>> {
        self.value_of_lossy("replace").map(|s| s.into_bytes())
    }

    /// Returns the unescaped context separator in UTF-8 bytes.
    fn context_separator(&self) -> Vec<u8> {
        match self.value_of_lossy("context-separator") {
            None => b"--".to_vec(),
            Some(sep) => unescape(&sep),
        }
    }

    /// Returns the preprocessor command
    fn preprocessor(&self) -> Option<PathBuf> {
        if let Some(path) = self.value_of_os("pre") {
            if path.is_empty() {
                None
            } else {
                Some(Path::new(path).to_path_buf())
            }
        } else {
            None
        }
    }

    /// Returns the unescaped path separator in UTF-8 bytes.
    fn path_separator(&self) -> Result<Option<u8>> {
        match self.value_of_lossy("path-separator") {
            None => Ok(None),
            Some(sep) => {
                let sep = unescape(&sep);
                if sep.is_empty() {
                    Ok(None)
                } else if sep.len() > 1 {
                    Err(From::from(format!(
                        "A path separator must be exactly one byte, but \
                         the given separator is {} bytes: {}\n\
                         In some shells on Windows '/' is automatically \
                         expanded. Use '//' instead.",
                         sep.len(),
                         escape(&sep),
                    )))
                } else {
                    Ok(Some(sep[0]))
                }
            }
        }
    }

    /// Returns the before and after contexts from the command line.
    ///
    /// If a context setting was absent, then `0` is returned.
    ///
    /// If there was a problem parsing the values from the user as an integer,
    /// then an error is returned.
    fn contexts(&self) -> Result<(usize, usize)> {
        let after = self.usize_of("after-context")?.unwrap_or(0);
        let before = self.usize_of("before-context")?.unwrap_or(0);
        let both = self.usize_of("context")?.unwrap_or(0);
        Ok(if both > 0 {
            (both, both)
        } else {
            (before, after)
        })
    }

    /// Returns whether the -c/--count or the --count-matches flags were
    /// passed from the command line.
    ///
    /// If --count-matches and --invert-match were passed in, behave
    /// as if --count and --invert-match were passed in (i.e. rg will
    /// count inverted matches as per existing behavior).
    fn counts(&self) -> (bool, bool) {
        let count = self.is_present("count");
        let count_matches = self.is_present("count-matches");
        let invert_matches = self.is_present("invert-match");
        let only_matching = self.is_present("only-matching");
        if count_matches && invert_matches {
            // Treat `-v --count-matches` as `-v -c`.
            (true, false)
        } else if count && only_matching {
            // Treat `-c --only-matching` as `--count-matches`.
            (false, true)
        } else {
            (count, count_matches)
        }
    }

    /// Returns the user's color choice based on command line parameters and
    /// environment.
    fn color_choice(&self) -> termcolor::ColorChoice {
        let preference = match self.value_of_lossy("color") {
            None => "auto".to_string(),
            Some(v) => v,
        };
        if preference == "always" {
            termcolor::ColorChoice::Always
        } else if preference == "ansi" {
            termcolor::ColorChoice::AlwaysAnsi
        } else if preference == "auto" {
            if atty::is(atty::Stream::Stdout) || self.is_present("pretty") {
                termcolor::ColorChoice::Auto
            } else {
                termcolor::ColorChoice::Never
            }
        } else {
            termcolor::ColorChoice::Never
        }
    }

    /// Returns the color specifications given by the user on the CLI.
    ///
    /// If the was a problem parsing any of the provided specs, then an error
    /// is returned.
    fn color_specs(&self) -> Result<ColorSpecs> {
        // Start with a default set of color specs.
        let mut specs = vec![
            #[cfg(unix)]
            "path:fg:magenta".parse().unwrap(),
            #[cfg(windows)]
            "path:fg:cyan".parse().unwrap(),
            "line:fg:green".parse().unwrap(),
            "match:fg:red".parse().unwrap(),
            "match:style:bold".parse().unwrap(),
        ];
        for spec_str in self.values_of_lossy_vec("colors") {
            specs.push(spec_str.parse()?);
        }
        Ok(ColorSpecs::new(&specs))
    }

    fn user_color_specs(&self) -> Result<Vec<grep_printer::UserColorSpec>> {
        // Start with a default set of color specs.
        let mut specs = vec![
            #[cfg(unix)]
            "path:fg:magenta".parse().unwrap(),
            #[cfg(windows)]
            "path:fg:cyan".parse().unwrap(),
            "line:fg:green".parse().unwrap(),
            "match:fg:red".parse().unwrap(),
            "match:style:bold".parse().unwrap(),
        ];
        for spec_str in self.values_of_lossy_vec("colors") {
            specs.push(spec_str.parse()?);
        }
        Ok(specs)
    }

    /// Return the text encoding specified.
    ///
    /// If the label given by the caller doesn't correspond to a valid
    /// supported encoding (and isn't `auto`), then return an error.
    ///
    /// A `None` encoding implies that the encoding should be automatically
    /// detected on a per-file basis.
    fn encoding(&self) -> Result<Option<&'static Encoding>> {
        match self.value_of_lossy("encoding") {
            None => Ok(None),
            Some(label) => {
                if label == "auto" {
                    return Ok(None);
                }
                match Encoding::for_label_no_replacement(label.as_bytes()) {
                    Some(enc) => Ok(Some(enc)),
                    None => Err(From::from(
                        format!("unsupported encoding: {}", label))),
                }
            }
        }
    }

    /// Returns whether status should be tracked for this run of ripgrep

    /// This is automatically disabled if we're asked to only list the
    /// files that wil be searched, files with matches or files
    /// without matches.
    fn stats(&self) -> bool {
        if self.is_present("files-with-matches") ||
           self.is_present("files-without-match") {
               return false;
        }
        self.is_present("stats")
    }

    /// Returns the approximate number of threads that ripgrep should use.
    fn threads(&self) -> Result<usize> {
        if self.is_present("sort-files") {
            return Ok(1);
        }
        let threads = self.usize_of("threads")?.unwrap_or(0);
        Ok(if threads == 0 {
            cmp::min(12, num_cpus::get())
        } else {
            threads
        })
    }

    fn matcher2(&self) -> Result<(grep_regex::RegexMatcher, bool)> {
        let smart =
            self.is_present("smart-case")
            && !self.is_present("ignore-case")
            && !self.is_present("case-sensitive");
        let casei =
            self.is_present("ignore-case")
            && !self.is_present("case-sensitive");

        let mut builder = grep_regex::RegexMatcherBuilder::new();
        builder
            .case_insensitive(casei)
            .case_smart(smart)
            .multi_line(true);
        if !self.is_present("multiline") {
            builder.line_terminator(Some(b'\n'));
        }
        if self.is_present("word-regexp") {
            builder.word(true);
        }
        if let Some(limit) = self.dfa_size_limit()? {
            builder.dfa_size_limit(limit);
        }
        if let Some(limit) = self.regex_size_limit()? {
            builder.size_limit(limit);
        }

        let pats = self.patterns()?;
        let ok = !pats.is_empty();
        Ok((builder.build(&pats.join("|"))?, ok))
    }

    /// Builds a grep matcher from the command line flags.
    ///
    /// If there was a problem extracting the pattern from the command line
    /// flags, then an error is returned.
    ///
    /// If no match can ever occur, then `false` is returned. Otherwise,
    /// `true` is returned.
    fn grep(&self) -> Result<(Grep, bool)> {
        let smart =
            self.is_present("smart-case")
            && !self.is_present("ignore-case")
            && !self.is_present("case-sensitive");
        let casei =
            self.is_present("ignore-case")
            && !self.is_present("case-sensitive");
        let pats = self.patterns()?;
        let ok = !pats.is_empty();
        let mut gb = GrepBuilder::new(&pats.join("|"))
            .case_smart(smart)
            .case_insensitive(casei)
            .line_terminator(b'\n');

        if let Some(limit) = self.dfa_size_limit()? {
            gb = gb.dfa_size_limit(limit);
        }
        if let Some(limit) = self.regex_size_limit()? {
            gb = gb.size_limit(limit);
        }
        Ok((gb.build()?, ok))
    }

    /// Builds the set of glob overrides from the command line flags.
    fn overrides(&self) -> Result<Override> {
        let mut ovr = OverrideBuilder::new(env::current_dir()?);
        for glob in self.values_of_lossy_vec("glob") {
            ovr.add(&glob)?;
        }
        // this is smelly. In the long run it might make sense
        // to change overridebuilder to be like globsetbuilder
        // but this would be a breaking change to the ignore crate
        // so it is being shelved for now...
        ovr.case_insensitive(true)?;
        for glob in self.values_of_lossy_vec("iglob") {
            ovr.add(&glob)?;
        }
        ovr.build().map_err(From::from)
    }

    /// Builds a file type matcher from the command line flags.
    fn types(&self) -> Result<Types> {
        let mut btypes = TypesBuilder::new();
        btypes.add_defaults();
        for ty in self.values_of_lossy_vec("type-clear") {
            btypes.clear(&ty);
        }
        for def in self.values_of_lossy_vec("type-add") {
            btypes.add_def(&def)?;
        }
        for ty in self.values_of_lossy_vec("type") {
            btypes.select(&ty);
        }
        for ty in self.values_of_lossy_vec("type-not") {
            btypes.negate(&ty);
        }
        btypes.build().map_err(From::from)
    }

    /// Parses an argument of the form `[0-9]+(KMG)?`.
    ///
    /// This always returns the result as a type `u64`. This must be converted
    /// to the appropriate type by the caller.
    fn parse_human_readable_size_arg(
        &self,
        arg_name: &str,
    ) -> Result<Option<u64>> {
        let arg_value = match self.value_of_lossy(arg_name) {
            Some(x) => x,
            None => return Ok(None)
        };
        let re = regex::Regex::new("^([0-9]+)([KMG])?$").unwrap();
        let caps =
            re.captures(&arg_value).ok_or_else(|| {
                format!("invalid format for {}", arg_name)
            })?;

        let value = caps[1].parse::<u64>()?;
        let suffix = caps.get(2).map(|x| x.as_str());

        let v_10 = value.checked_mul(1024);
        let v_20 = v_10.and_then(|x| x.checked_mul(1024));
        let v_30 = v_20.and_then(|x| x.checked_mul(1024));

        let try_suffix = |x: Option<u64>| {
            if x.is_some() {
                Ok(x)
            } else {
                Err(From::from(format!("number too large for {}", arg_name)))
            }
        };
        match suffix {
            None      => Ok(Some(value)),
            Some("K") => try_suffix(v_10),
            Some("M") => try_suffix(v_20),
            Some("G") => try_suffix(v_30),
            _ => Err(From::from(format!("invalid suffix for {}", arg_name)))
        }
    }

    /// Parse the dfa-size-limit argument option into a byte count.
    fn dfa_size_limit(&self) -> Result<Option<usize>> {
        let r = self.parse_human_readable_size_arg("dfa-size-limit")?;
        human_readable_to_usize("dfa-size-limit", r)
    }

    /// Parse the regex-size-limit argument option into a byte count.
    fn regex_size_limit(&self) -> Result<Option<usize>> {
        let r = self.parse_human_readable_size_arg("regex-size-limit")?;
        human_readable_to_usize("regex-size-limit", r)
    }

    /// Parses the max-filesize argument option into a byte count.
    fn max_filesize(&self) -> Result<Option<u64>> {
        self.parse_human_readable_size_arg("max-filesize")
    }

    /// Returns true if ignore files should be ignored.
    fn no_ignore(&self) -> bool {
        self.is_present("no-ignore")
        || self.occurrences_of("unrestricted") >= 1
    }

    /// Returns true if global ignore files should be ignored.
    fn no_ignore_global(&self) -> bool {
        self.is_present("no-ignore-global") || self.no_ignore()
    }

    /// Returns true if parent ignore files should be ignored.
    fn no_ignore_parent(&self) -> bool {
        self.is_present("no-ignore-parent") || self.no_ignore()
    }

    /// Returns true if VCS ignore files should be ignored.
    fn no_ignore_vcs(&self) -> bool {
        self.is_present("no-ignore-vcs") || self.no_ignore()
    }

    /// Returns true if and only if hidden files/directories should be
    /// searched.
    fn hidden(&self) -> bool {
        self.is_present("hidden") || self.occurrences_of("unrestricted") >= 2
    }

    /// Returns true if and only if all files should be treated as if they
    /// were text, even if ripgrep would detect it as a binary file.
    fn text(&self) -> bool {
        self.is_present("text") || self.occurrences_of("unrestricted") >= 3
    }

    /// Like values_of_lossy, but returns an empty vec if the flag is not
    /// present.
    fn values_of_lossy_vec(&self, name: &str) -> Vec<String> {
        self.values_of_lossy(name).unwrap_or_else(Vec::new)
    }

    /// Safely reads an arg value with the given name, and if it's present,
    /// tries to parse it as a usize value.
    ///
    /// If the number is zero, then it is considered absent and `None` is
    /// returned.
    fn usize_of_nonzero(&self, name: &str) -> Result<Option<usize>> {
        match self.value_of_lossy(name) {
            None => Ok(None),
            Some(v) => v.parse().map_err(From::from).map(|n| {
                if n == 0 {
                    None
                } else {
                    Some(n)
                }
            }),
        }
    }

    /// Safely reads an arg value with the given name, and if it's present,
    /// tries to parse it as a usize value.
    fn usize_of(&self, name: &str) -> Result<Option<usize>> {
        match self.value_of_lossy(name) {
            None => Ok(None),
            Some(v) => v.parse().map(Some).map_err(From::from),
        }
    }

    // The following methods mostly dispatch to the underlying clap methods
    // directly. Methods that would otherwise get a single value will fetch
    // all values and return the last one. (Clap returns the first one.) We
    // only define the ones we need.

    fn is_present(&self, name: &str) -> bool {
        self.0.is_present(name)
    }

    fn occurrences_of(&self, name: &str) -> u64 {
        self.0.occurrences_of(name)
    }

    fn value_of_lossy(&self, name: &str) -> Option<String> {
        self.0.value_of_lossy(name).map(|s| s.into_owned())
    }

    fn values_of_lossy(&self, name: &str) -> Option<Vec<String>> {
        self.0.values_of_lossy(name)
    }

    fn value_of_os(&'a self, name: &str) -> Option<&'a OsStr> {
        self.0.value_of_os(name)
    }

    fn values_of_os(&'a self, name: &str) -> Option<clap::OsValues<'a>> {
        self.0.values_of_os(name)
    }
}

fn pattern_to_str(s: &OsStr) -> Result<&str> {
    match s.to_str() {
        Some(s) => Ok(s),
        None => Err(From::from(format!(
            "Argument '{}' is not valid UTF-8. \
             Use hex escape sequences to match arbitrary \
             bytes in a pattern (e.g., \\xFF).",
             s.to_string_lossy()))),
    }
}

/// A simple thread safe abstraction for determining whether a search should
/// stop if the user has requested quiet mode.
#[derive(Clone, Debug)]
pub struct QuietMatched(Arc<Option<AtomicBool>>);

impl QuietMatched {
    /// Create a new QuietMatched value.
    ///
    /// If quiet is true, then set_match and has_match will reflect whether
    /// a search should quit or not because it found a match.
    ///
    /// If quiet is false, then set_match is always a no-op and has_match
    /// always returns false.
    fn new(quiet: bool) -> QuietMatched {
        let atomic = if quiet { Some(AtomicBool::new(false)) } else { None };
        QuietMatched(Arc::new(atomic))
    }

    /// Returns true if and only if quiet mode is enabled and a match has
    /// occurred.
    pub fn has_match(&self) -> bool {
        match *self.0 {
            None => false,
            Some(ref matched) => matched.load(Ordering::SeqCst),
        }
    }

    /// Sets whether a match has occurred or not.
    ///
    /// If quiet mode is disabled, then this is a no-op.
    pub fn set_match(&self, yes: bool) -> bool {
        match *self.0 {
            None => false,
            Some(_) if !yes => false,
            Some(ref m) => { m.store(true, Ordering::SeqCst); true }
        }
    }
}

/// Convert the result of a `parse_human_readable_size_arg` call into
/// a `usize`, failing if the type does not fit.
fn human_readable_to_usize(
    arg_name: &str,
    value: Option<u64>,
) -> Result<Option<usize>> {
    use std::usize;

    match value {
        None => Ok(None),
        Some(v) => {
            if v <= usize::MAX as u64 {
                Ok(Some(v as usize))
            } else {
                let msg = format!("number too large for {}", arg_name);
                Err(From::from(msg))
            }
        }
    }
}

/// Returns true if and only if stdin is deemed searchable.
#[cfg(unix)]
fn stdin_is_readable() -> bool {
    use std::os::unix::fs::FileTypeExt;
    use same_file::Handle;

    let ft = match Handle::stdin().and_then(|h| h.as_file().metadata()) {
        Err(_) => return false,
        Ok(md) => md.file_type(),
    };
    ft.is_file() || ft.is_fifo()
}

/// Returns true if and only if stdin is deemed searchable.
#[cfg(windows)]
fn stdin_is_readable() -> bool {
    // On Windows, it's not clear what the possibilities are to me, so just
    // always return true.
    true
}

/// Returns true if and only if this path points to a directory.
///
/// This works around a bug in Rust's standard library:
/// https://github.com/rust-lang/rust/issues/46484
#[cfg(windows)]
fn path_is_dir(path: &Path) -> bool {
    fs::metadata(path).map(|md| metadata_is_dir(&md)).unwrap_or(false)
}

/// Returns true if and only if this entry points to a directory.
#[cfg(not(windows))]
fn path_is_dir(path: &Path) -> bool {
    path.is_dir()
}

/// Returns true if and only if this path points to a file.
///
/// This works around a bug in Rust's standard library:
/// https://github.com/rust-lang/rust/issues/46484
#[cfg(windows)]
fn path_is_file(path: &Path) -> bool {
    !path_is_dir(path)
}

/// Returns true if and only if this entry points to a directory.
#[cfg(not(windows))]
fn path_is_file(path: &Path) -> bool {
    path.is_file()
}

/// Returns true if and only if the given metadata points to a directory.
///
/// This works around a bug in Rust's standard library:
/// https://github.com/rust-lang/rust/issues/46484
#[cfg(windows)]
fn metadata_is_dir(md: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    use winapi::um::winnt::FILE_ATTRIBUTE_DIRECTORY;
    md.file_attributes() & FILE_ATTRIBUTE_DIRECTORY != 0
}
