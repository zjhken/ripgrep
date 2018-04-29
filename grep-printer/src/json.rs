use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use grep_matcher::{Match, Matcher};
use grep_searcher::{Searcher, Sink, SinkError, SinkMatch};
use serde_json;

use counter::CounterWriter;
use jsont;
use stats::Stats;

/// The configuration for the standard printer.
///
/// This is manipulated by the StandardBuilder and then referenced by the
/// actual implementation. Once a printer is build, the configuration is frozen
/// and cannot changed.
#[derive(Debug, Clone)]
struct Config {
    max_matches: Option<u64>,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            max_matches: None,
        }
    }
}

/// A builder for a JSON lines printer.
///
/// The builder permits configuring how the printer behaves. The JSON printer
/// has fewer configuration options than the standard printer because it is
/// a structured format, and the printer always attempts to find the most
/// information possible.
///
/// One a printer is built, its configuration cannot be changed.
#[derive(Clone, Debug)]
pub struct JSONBuilder {
    config: Config,
}

impl JSONBuilder {
    /// Return a new builder for configuring the JSON printer.
    pub fn new() -> JSONBuilder {
        JSONBuilder { config: Config::default() }
    }

    /// Create a JSON printer that writes results to the given writer.
    pub fn build<W: io::Write>(&self, wtr: W) -> JSON<W> {
        JSON {
            config: self.config.clone(),
            wtr: CounterWriter::new(wtr),
            matches: vec![],
            stats: Stats::new()
        }
    }

    /// Set the maximum amount of matches that are printed.
    ///
    /// If multi line search is enabled and a match spans multiple lines, then
    /// that match is counted exactly once for the purposes of enforcing this
    /// limit, regardless of how many lines it spans.
    pub fn max_matches(&mut self, limit: Option<u64>) -> &mut JSONBuilder {
        self.config.max_matches = limit;
        self
    }
}

/// The JSON printer, which emits results in a JSON lines format.
#[derive(Debug)]
pub struct JSON<W> {
    config: Config,
    wtr: CounterWriter<W>,
    matches: Vec<Match>,
    stats: Stats,
}

impl<W: io::Write> JSON<W> {
    /// Return a JSON lines printer with a default configuration that writes
    /// matches to the given writer.
    pub fn new(wtr: W) -> JSON<W> {
        JSONBuilder::new().build(wtr)
    }

    /// Return an implementation of `Sink` for the JSON printer.
    ///
    /// This does not associate the printer with a file path, which means this
    /// implementation will never print a file path along with the matches.
    pub fn sink<'s, M: Matcher>(
        &'s mut self,
        matcher: M,
    ) -> JSONSink<'static, 's, M, W> {
        JSONSink {
            matcher: matcher,
            json: self,
            path: None,
            start_time: Instant::now(),
            match_count: 0,
            after_context_remaining: 0,
            binary_byte_offset: None,
        }
    }

    /// Return an implementation of `Sink` associated with a file path.
    ///
    /// When the printer is associated with a path, then it may, depending on
    /// its configuration, print the path along with the matches found.
    pub fn sink_with_path<'p, 's, M, P>(
        &'s mut self,
        matcher: M,
        path: &'p P,
    ) -> JSONSink<'p, 's, M, W>
    where M: Matcher,
          P: ?Sized + AsRef<Path>,
    {
        JSONSink {
            matcher: matcher,
            json: self,
            path: Some(path.as_ref()),
            start_time: Instant::now(),
            match_count: 0,
            after_context_remaining: 0,
            binary_byte_offset: None,
        }
    }

    /// Return a mutable reference to the underlying writer.
    pub fn get_mut(&mut self) -> &mut W {
        self.wtr.get_mut()
    }

    /// Consume this printer and return back ownership of the underlying
    /// writer.
    pub fn into_inner(self) -> W {
        self.wtr.into_inner()
    }

    /// Return a reference to the stats produced by the printer. The stats
    /// returned are cumulative over all searches performed using this printer.
    pub fn stats(&self) -> &Stats {
        &self.stats
    }
}

/// An implementation of `Sink` associated with a matcher and an optional file
/// path for the JSON printer.
#[derive(Debug)]
pub struct JSONSink<'p, 's, M: Matcher, W: 's> {
    matcher: M,
    json: &'s mut JSON<W>,
    path: Option<&'p Path>,
    start_time: Instant,
    match_count: u64,
    after_context_remaining: u64,
    binary_byte_offset: Option<u64>,
}

impl<'p, 's, M: Matcher, W: io::Write> JSONSink<'p, 's, M, W> {
    /// Returns true if and only if this printer received a match in the
    /// previous search.
    ///
    /// This is unaffected by the result of searches before the previous
    /// search.
    pub fn has_match(&self) -> bool {
        self.match_count > 0
    }

    /// If binary data was found in the previous search, this returns the
    /// offset at which the binary data was first detected.
    ///
    /// The offset returned is an absolute offset relative to the entire
    /// set of bytes searched.
    ///
    /// This is unaffected by the result of searches before the previous
    /// search. e.g., If the search prior to the previous search found binary
    /// data but the previous search found no binary data, then this will
    /// return `None`.
    pub fn binary_byte_offset(&self) -> Option<u64> {
        self.binary_byte_offset
    }

    /// Execute the matcher over the given bytes and record the match
    /// locations if the current configuration demands match granularity.
    fn record_matches(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.json.matches.clear();
        // If printing requires knowing the location of each individual match,
        // then compute and stored those right now for use later. While this
        // adds an extra copy for storing the matches, we do amortize the
        // allocation for it and this greatly simplifies the printing logic to
        // the extent that it's easy to ensure that we never do more than
        // one search to find the matches (well, for replacements, we do one
        // additional search to perform the actual replacement).
        let matches = &mut self.json.matches;
        self.matcher.find_iter(bytes, |m| {
            matches.push(m);
            true
        }).map_err(io::Error::error_message)?;
        Ok(())
    }

    /// Returns true if this printer should quit.
    ///
    /// This implements the logic for handling quitting after seeing a certain
    /// amount of matches. In most cases, the logic is simple, but we must
    /// permit all "after" contextual lines to print after reaching the limit.
    fn should_quit(&self) -> bool {
        let limit = match self.json.config.max_matches {
            None => return false,
            Some(limit) => limit,
        };
        if self.match_count < limit {
            return false;
        }
        self.after_context_remaining == 0
    }
}

impl<'p, 's, M: Matcher, W: io::Write> Sink for JSONSink<'p, 's, M, W> {
    type Error = io::Error;

    fn matched(
        &mut self,
        searcher: &Searcher,
        mat: &SinkMatch,
    ) -> Result<bool, io::Error> {
        self.match_count += 1;
        self.after_context_remaining = searcher.after_context() as u64;
        self.record_matches(mat.bytes())?;
        self.json.stats.add_matches(self.json.matches.len() as u64);
        self.json.stats.add_matched_lines(mat.lines().count() as u64);

        let match_ranges = MatchRanges::new(mat.bytes(), &self.json.matches);
        let msg = jsont::Message::Matched(jsont::Matched {
            path: self.path,
            lines: mat.bytes(),
            line_number: mat.line_number(),
            absolute_offset: mat.absolute_byte_offset(),
            matches: match_ranges.as_slice(),
        });
        serde_json::to_writer(&mut self.json.wtr, &msg)?;
        self.json.wtr.write(&[searcher.line_terminator()])?;
        Ok(!self.should_quit())
    }
}

enum MatchRanges<'a> {
    Small([jsont::MatchRange<'a>; 1]),
    Big(Vec<jsont::MatchRange<'a>>),
}

impl<'a> MatchRanges<'a> {
    fn new(bytes: &'a[u8], matches: &[Match]) -> MatchRanges<'a> {
        if matches.len() == 1 {
            let mat = matches[0];
            MatchRanges::Small([jsont::MatchRange {
                m: &bytes[mat],
                start: mat.start(),
                end: mat.end(),
            }])
        } else {
            let mut match_ranges = vec![];
            for &mat in matches {
                match_ranges.push(jsont::MatchRange {
                    m: &bytes[mat],
                    start: mat.start(),
                    end: mat.end(),
                });
            }
            MatchRanges::Big(match_ranges)
        }
    }

    fn as_slice(&self) -> &[jsont::MatchRange] {
        match *self {
            MatchRanges::Small(ref x) => x,
            MatchRanges::Big(ref x) => x,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use grep_regex::RegexMatcher;
    use grep_searcher::SearcherBuilder;

    use super::{JSON, JSONBuilder};

    const SHERLOCK: &'static [u8] = b"\
For the Doc\xFFtor Watsons of this world, as opposed to the Sherlock
Holmeses, success in the province of detective work must always
be, to a very large extent, the result of luck. Sherlock Holmes
can extract a clew from a wisp of straw or a flake of cigar ash;
but Doctor Watson has to have it taken out for him and dusted,
and exhibited clearly, with a label attached.\
";

    fn printer_contents(
        printer: &mut JSON<Vec<u8>>,
    ) -> String {
        String::from_utf8(printer.get_mut().to_owned()).unwrap()
    }

    /*
    #[test]
    fn scratch() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        // let raw = OsStr::from_bytes(b"/home/and\xFFrew/rust/ripgrep");
        // let path = PathBuf::from(raw);
        let path = PathBuf::from("/home/andrew/rust/ripgrep");
        let msg = Message::Begin(Begin {
            path: Some(path),
        });
        let out = json::to_string_pretty(&msg).unwrap();
        println!("{}", out);
    }
    */

    #[test]
    fn scratch() {
        let matcher = RegexMatcher::new(
            r"Holmeses|Watson|Sherlock"
        ).unwrap();
        let mut printer = JSONBuilder::new()
            .build(vec![]);
        SearcherBuilder::new()
            .line_number(true)
            .build()
            .search_reader(
                &matcher,
                SHERLOCK,
                printer.sink_with_path(&matcher, Path::new("sherlock")),
            )
            .unwrap();
        let got = printer_contents(&mut printer);

        println!("{}", got);
    }
}
