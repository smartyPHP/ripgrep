/*!
The globset crate provides cross platform single glob and glob set matching.

Glob set matching is the process of matching one or more glob patterns against
a single candidate path simultaneously, and returning all of the globs that
matched. For example, given this set of globs:

```ignore
*.rs
src/lib.rs
src/**/foo.rs
```

and a path `src/bar/baz/foo.rs`, then the set would report the first and third
globs as matching.

# Example: one glob

This example shows how to match a single glob against a single file path.

```
# fn example() -> Result<(), globset::Error> {
use globset::Glob;

let glob = try!(Glob::new("*.rs")).compile_matcher();

assert!(glob.is_match("foo.rs"));
assert!(glob.is_match("foo/bar.rs"));
assert!(!glob.is_match("Cargo.toml"));
# Ok(()) } example().unwrap();
```

# Example: configuring a glob matcher

This example shows how to use a `GlobBuilder` to configure aspects of match
semantics. In this example, we prevent wildcards from matching path separators.

```
# fn example() -> Result<(), globset::Error> {
use globset::GlobBuilder;

let glob = try!(GlobBuilder::new("*.rs")
    .literal_separator(true).build()).compile_matcher();

assert!(glob.is_match("foo.rs"));
assert!(!glob.is_match("foo/bar.rs")); // no longer matches
assert!(!glob.is_match("Cargo.toml"));
# Ok(()) } example().unwrap();
```

# Example: match multiple globs at once

This example shows how to match multiple glob patterns at once.

```
# fn example() -> Result<(), globset::Error> {
use globset::{Glob, GlobSetBuilder};

let mut builder = GlobSetBuilder::new();
// A GlobBuilder can be used to configure each glob's match semantics
// independently.
builder.add(try!(Glob::new("*.rs")));
builder.add(try!(Glob::new("src/lib.rs")));
builder.add(try!(Glob::new("src/**/foo.rs")));
let set = try!(builder.build());

assert_eq!(set.matches("src/bar/baz/foo.rs"), vec![0, 2]);
# Ok(()) } example().unwrap();
```

# Syntax

Standard Unix-style glob syntax is supported:

* `?` matches any single character. (If the `literal_separator` option is
  enabled, then `?` can never match a path separator.)
* `*` matches zero or more characters. (If the `literal_separator` option is
  enabled, then `*` can never match a path separator.)
* `**` recursively matches directories but are only legal in three situations.
  First, if the glob starts with <code>\*\*&#x2F;</code>, then it matches
  all directories. For example, <code>\*\*&#x2F;foo</code> matches `foo`
  and `bar/foo` but not `foo/bar`. Secondly, if the glob ends with
  <code>&#x2F;\*\*</code>, then it matches all sub-entries. For example,
  <code>foo&#x2F;\*\*</code> matches `foo/a` and `foo/a/b`, but not `foo`.
  Thirdly, if the glob contains <code>&#x2F;\*\*&#x2F;</code> anywhere within
  the pattern, then it matches zero or more directories. Using `**` anywhere
  else is illegal (N.B. the glob `**` is allowed and means "match everything").
* `{a,b}` matches `a` or `b` where `a` and `b` are arbitrary glob patterns.
  (N.B. Nesting `{...}` is not currently allowed.)
* `[ab]` matches `a` or `b` where `a` and `b` are characters. Use
  `[!ab]` to match any character except for `a` and `b`.
* Metacharacters such as `*` and `?` can be escaped with character class
  notation. e.g., `[*]` matches `*`.

A `GlobBuilder` can be used to prevent wildcards from matching path separators,
or to enable case insensitive matching.
*/

#![deny(missing_docs)]

extern crate aho_corasick;
extern crate fnv;
#[macro_use]
extern crate log;
extern crate memchr;
extern crate regex;

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::error::Error as StdError;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::hash;
use std::path::Path;
use std::str;

use aho_corasick::{Automaton, AcAutomaton, FullAcAutomaton};
use regex::bytes::{Regex, RegexBuilder, RegexSet};

use pathutil::{
    file_name, file_name_ext, normalize_path, os_str_bytes, path_bytes,
};
use glob::MatchStrategy;
pub use glob::{Glob, GlobBuilder, GlobMatcher};

mod glob;
mod pathutil;

/// Represents an error that can occur when parsing a glob pattern.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Error {
    /// Occurs when a use of `**` is invalid. Namely, `**` can only appear
    /// adjacent to a path separator, or the beginning/end of a glob.
    InvalidRecursive,
    /// Occurs when a character class (e.g., `[abc]`) is not closed.
    UnclosedClass,
    /// Occurs when a range in a character (e.g., `[a-z]`) is invalid. For
    /// example, if the range starts with a lexicographically larger character
    /// than it ends with.
    InvalidRange(char, char),
    /// Occurs when a `}` is found without a matching `{`.
    UnopenedAlternates,
    /// Occurs when a `{` is found without a matching `}`.
    UnclosedAlternates,
    /// Occurs when an alternating group is nested inside another alternating
    /// group, e.g., `{{a,b},{c,d}}`.
    NestedAlternates,
    /// An error associated with parsing or compiling a regex.
    Regex(String),
}

impl StdError for Error {
    fn description(&self) -> &str {
        match *self {
            Error::InvalidRecursive => {
                "invalid use of **; must be one path component"
            }
            Error::UnclosedClass => {
                "unclosed character class; missing ']'"
            }
            Error::InvalidRange(_, _) => {
                "invalid character range"
            }
            Error::UnopenedAlternates => {
                "unopened alternate group; missing '{' \
                (maybe escape '}' with '[}]'?)"
            }
            Error::UnclosedAlternates => {
                "unclosed alternate group; missing '}' \
                (maybe escape '{' with '[{]'?)"
            }
            Error::NestedAlternates => {
                "nested alternate groups are not allowed"
            }
            Error::Regex(ref err) => err,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            Error::InvalidRecursive
            | Error::UnclosedClass
            | Error::UnopenedAlternates
            | Error::UnclosedAlternates
            | Error::NestedAlternates
            | Error::Regex(_) => {
                write!(f, "{}", self.description())
            }
            Error::InvalidRange(s, e) => {
                write!(f, "invalid range; '{}' > '{}'", s, e)
            }
        }
    }
}

fn new_regex(pat: &str) -> Result<Regex, Error> {
    RegexBuilder::new(pat)
        .dot_matches_new_line(true)
        .size_limit(10 * (1 << 20))
        .dfa_size_limit(10 * (1 << 20))
        .build()
        .map_err(|err| Error::Regex(err.to_string()))
}

fn new_regex_set<I, S>(pats: I) -> Result<RegexSet, Error>
        where S: AsRef<str>, I: IntoIterator<Item=S> {
    RegexSet::new(pats).map_err(|err| Error::Regex(err.to_string()))
}

type Fnv = hash::BuildHasherDefault<fnv::FnvHasher>;

/// GlobSet represents a group of globs that can be matched together in a
/// single pass.
#[derive(Clone, Debug)]
pub struct GlobSet {
    len: usize,
    strats: Vec<GlobSetMatchStrategy>,
}

impl GlobSet {
    /// Returns true if this set is empty, and therefore matches nothing.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Returns the number of globs in this set.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Returns true if any glob in this set matches the path given.
    pub fn is_match<P: AsRef<Path>>(&self, path: P) -> bool {
        self.is_match_candidate(&Candidate::new(path.as_ref()))
    }

    /// Returns true if any glob in this set matches the path given.
    ///
    /// This takes a Candidate as input, which can be used to amortize the
    /// cost of preparing a path for matching.
    pub fn is_match_candidate(&self, path: &Candidate) -> bool {
        if self.is_empty() {
            return false;
        }
        for strat in &self.strats {
            if strat.is_match(path) {
                return true;
            }
        }
        false
    }

    /// Returns the sequence number of every glob pattern that matches the
    /// given path.
    pub fn matches<P: AsRef<Path>>(&self, path: P) -> Vec<usize> {
        self.matches_candidate(&Candidate::new(path.as_ref()))
    }

    /// Returns the sequence number of every glob pattern that matches the
    /// given path.
    ///
    /// This takes a Candidate as input, which can be used to amortize the
    /// cost of preparing a path for matching.
    pub fn matches_candidate(&self, path: &Candidate) -> Vec<usize> {
        let mut into = vec![];
        if self.is_empty() {
            return into;
        }
        self.matches_candidate_into(path, &mut into);
        into
    }

    /// Adds the sequence number of every glob pattern that matches the given
    /// path to the vec given.
    ///
    /// `into` is is cleared before matching begins, and contains the set of
    /// sequence numbers (in ascending order) after matching ends. If no globs
    /// were matched, then `into` will be empty.
    pub fn matches_into<P: AsRef<Path>>(
        &self,
        path: P,
        into: &mut Vec<usize>,
    ) {
        self.matches_candidate_into(&Candidate::new(path.as_ref()), into);
    }

    /// Adds the sequence number of every glob pattern that matches the given
    /// path to the vec given.
    ///
    /// `into` is is cleared before matching begins, and contains the set of
    /// sequence numbers (in ascending order) after matching ends. If no globs
    /// were matched, then `into` will be empty.
    ///
    /// This takes a Candidate as input, which can be used to amortize the
    /// cost of preparing a path for matching.
    pub fn matches_candidate_into(
        &self,
        path: &Candidate,
        into: &mut Vec<usize>,
    ) {
        into.clear();
        if self.is_empty() {
            return;
        }
        for strat in &self.strats {
            strat.matches_into(path, into);
        }
        into.sort();
        into.dedup();
    }

    fn new(pats: &[Glob]) -> Result<GlobSet, Error> {
        if pats.is_empty() {
            return Ok(GlobSet { len: 0, strats: vec![] });
        }
        let mut lits = LiteralStrategy::new();
        let mut base_lits = BasenameLiteralStrategy::new();
        let mut exts = ExtensionStrategy::new();
        let mut prefixes = MultiStrategyBuilder::new();
        let mut suffixes = MultiStrategyBuilder::new();
        let mut required_exts = RequiredExtensionStrategyBuilder::new();
        let mut regexes = MultiStrategyBuilder::new();
        for (i, p) in pats.iter().enumerate() {
            match MatchStrategy::new(p) {
                MatchStrategy::Literal(lit) => {
                    lits.add(i, lit);
                }
                MatchStrategy::BasenameLiteral(lit) => {
                    base_lits.add(i, lit);
                }
                MatchStrategy::Extension(ext) => {
                    exts.add(i, ext);
                }
                MatchStrategy::Prefix(prefix) => {
                    prefixes.add(i, prefix);
                }
                MatchStrategy::Suffix { suffix, component } => {
                    if component {
                        lits.add(i, suffix[1..].to_string());
                    }
                    suffixes.add(i, suffix);
                }
                MatchStrategy::RequiredExtension(ext) => {
                    required_exts.add(i, ext, p.regex().to_owned());
                }
                MatchStrategy::Regex => {
                    debug!("glob converted to regex: {:?}", p);
                    regexes.add(i, p.regex().to_owned());
                }
            }
        }
        debug!("built glob set; {} literals, {} basenames, {} extensions, \
                {} prefixes, {} suffixes, {} required extensions, {} regexes",
                lits.0.len(), base_lits.0.len(), exts.0.len(),
                prefixes.literals.len(), suffixes.literals.len(),
                required_exts.0.len(), regexes.literals.len());
        Ok(GlobSet {
            len: pats.len(),
            strats: vec![
                GlobSetMatchStrategy::Extension(exts),
                GlobSetMatchStrategy::BasenameLiteral(base_lits),
                GlobSetMatchStrategy::Literal(lits),
                GlobSetMatchStrategy::Suffix(suffixes.suffix()),
                GlobSetMatchStrategy::Prefix(prefixes.prefix()),
                GlobSetMatchStrategy::RequiredExtension(
                    try!(required_exts.build())),
                GlobSetMatchStrategy::Regex(try!(regexes.regex_set())),
            ],
        })
    }
}

/// GlobSetBuilder builds a group of patterns that can be used to
/// simultaneously match a file path.
pub struct GlobSetBuilder {
    pats: Vec<Glob>,
}

impl GlobSetBuilder {
    /// Create a new GlobSetBuilder. A GlobSetBuilder can be used to add new
    /// patterns. Once all patterns have been added, `build` should be called
    /// to produce a `GlobSet`, which can then be used for matching.
    pub fn new() -> GlobSetBuilder {
        GlobSetBuilder { pats: vec![] }
    }

    /// Builds a new matcher from all of the glob patterns added so far.
    ///
    /// Once a matcher is built, no new patterns can be added to it.
    pub fn build(&self) -> Result<GlobSet, Error> {
        GlobSet::new(&self.pats)
    }

    /// Add a new pattern to this set.
    #[allow(dead_code)]
    pub fn add(&mut self, pat: Glob) -> &mut GlobSetBuilder {
        self.pats.push(pat);
        self
    }
}

/// A candidate path for matching.
///
/// All glob matching in this crate operates on `Candidate` values.
/// Constructing candidates has a very small cost associated with it, so
/// callers may find it beneficial to amortize that cost when matching a single
/// path against multiple globs or sets of globs.
#[derive(Clone, Debug)]
pub struct Candidate<'a> {
    path: Cow<'a, [u8]>,
    basename: Cow<'a, [u8]>,
    ext: &'a OsStr,
}

impl<'a> Candidate<'a> {
    /// Create a new candidate for matching from the given path.
    pub fn new<P: AsRef<Path> + ?Sized>(path: &'a P) -> Candidate<'a> {
        let path = path.as_ref();
        let basename = file_name(path).unwrap_or(OsStr::new(""));
        Candidate {
            path: normalize_path(path_bytes(path)),
            basename: os_str_bytes(basename),
            ext: file_name_ext(basename).unwrap_or(OsStr::new("")),
        }
    }

    fn path_prefix(&self, max: usize) -> &[u8] {
        if self.path.len() <= max {
            &*self.path
        } else {
            &self.path[..max]
        }
    }

    fn path_suffix(&self, max: usize) -> &[u8] {
        if self.path.len() <= max {
            &*self.path
        } else {
            &self.path[self.path.len() - max..]
        }
    }
}

#[derive(Clone, Debug)]
enum GlobSetMatchStrategy {
    Literal(LiteralStrategy),
    BasenameLiteral(BasenameLiteralStrategy),
    Extension(ExtensionStrategy),
    Prefix(PrefixStrategy),
    Suffix(SuffixStrategy),
    RequiredExtension(RequiredExtensionStrategy),
    Regex(RegexSetStrategy),
}

impl GlobSetMatchStrategy {
    fn is_match(&self, candidate: &Candidate) -> bool {
        use self::GlobSetMatchStrategy::*;
        match *self {
            Literal(ref s) => s.is_match(candidate),
            BasenameLiteral(ref s) => s.is_match(candidate),
            Extension(ref s) => s.is_match(candidate),
            Prefix(ref s) => s.is_match(candidate),
            Suffix(ref s) => s.is_match(candidate),
            RequiredExtension(ref s) => s.is_match(candidate),
            Regex(ref s) => s.is_match(candidate),
        }
    }

    fn matches_into(&self, candidate: &Candidate, matches: &mut Vec<usize>) {
        use self::GlobSetMatchStrategy::*;
        match *self {
            Literal(ref s) => s.matches_into(candidate, matches),
            BasenameLiteral(ref s) => s.matches_into(candidate, matches),
            Extension(ref s) => s.matches_into(candidate, matches),
            Prefix(ref s) => s.matches_into(candidate, matches),
            Suffix(ref s) => s.matches_into(candidate, matches),
            RequiredExtension(ref s) => s.matches_into(candidate, matches),
            Regex(ref s) => s.matches_into(candidate, matches),
        }
    }
}

#[derive(Clone, Debug)]
struct LiteralStrategy(BTreeMap<Vec<u8>, Vec<usize>>);

impl LiteralStrategy {
    fn new() -> LiteralStrategy {
        LiteralStrategy(BTreeMap::new())
    }

    fn add(&mut self, global_index: usize, lit: String) {
        self.0.entry(lit.into_bytes()).or_insert(vec![]).push(global_index);
    }

    fn is_match(&self, candidate: &Candidate) -> bool {
        self.0.contains_key(&*candidate.path)
    }

    #[inline(never)]
    fn matches_into(&self, candidate: &Candidate, matches: &mut Vec<usize>) {
        if let Some(hits) = self.0.get(&*candidate.path) {
            matches.extend(hits);
        }
    }
}

#[derive(Clone, Debug)]
struct BasenameLiteralStrategy(BTreeMap<Vec<u8>, Vec<usize>>);

impl BasenameLiteralStrategy {
    fn new() -> BasenameLiteralStrategy {
        BasenameLiteralStrategy(BTreeMap::new())
    }

    fn add(&mut self, global_index: usize, lit: String) {
        self.0.entry(lit.into_bytes()).or_insert(vec![]).push(global_index);
    }

    fn is_match(&self, candidate: &Candidate) -> bool {
        if candidate.basename.is_empty() {
            return false;
        }
        self.0.contains_key(&*candidate.basename)
    }

    #[inline(never)]
    fn matches_into(&self, candidate: &Candidate, matches: &mut Vec<usize>) {
        if candidate.basename.is_empty() {
            return;
        }
        if let Some(hits) = self.0.get(&*candidate.basename) {
            matches.extend(hits);
        }
    }
}

#[derive(Clone, Debug)]
struct ExtensionStrategy(HashMap<OsString, Vec<usize>, Fnv>);

impl ExtensionStrategy {
    fn new() -> ExtensionStrategy {
        ExtensionStrategy(HashMap::with_hasher(Fnv::default()))
    }

    fn add(&mut self, global_index: usize, ext: OsString) {
        self.0.entry(ext).or_insert(vec![]).push(global_index);
    }

    fn is_match(&self, candidate: &Candidate) -> bool {
        if candidate.ext.is_empty() {
            return false;
        }
        self.0.contains_key(candidate.ext)
    }

    #[inline(never)]
    fn matches_into(&self, candidate: &Candidate, matches: &mut Vec<usize>) {
        if candidate.ext.is_empty() {
            return;
        }
        if let Some(hits) = self.0.get(candidate.ext) {
            matches.extend(hits);
        }
    }
}

#[derive(Clone, Debug)]
struct PrefixStrategy {
    matcher: FullAcAutomaton<Vec<u8>>,
    map: Vec<usize>,
    longest: usize,
}

impl PrefixStrategy {
    fn is_match(&self, candidate: &Candidate) -> bool {
        let path = candidate.path_prefix(self.longest);
        for m in self.matcher.find_overlapping(path) {
            if m.start == 0 {
                return true;
            }
        }
        false
    }

    fn matches_into(&self, candidate: &Candidate, matches: &mut Vec<usize>) {
        let path = candidate.path_prefix(self.longest);
        for m in self.matcher.find_overlapping(path) {
            if m.start == 0 {
                matches.push(self.map[m.pati]);
            }
        }
    }
}

#[derive(Clone, Debug)]
struct SuffixStrategy {
    matcher: FullAcAutomaton<Vec<u8>>,
    map: Vec<usize>,
    longest: usize,
}

impl SuffixStrategy {
    fn is_match(&self, candidate: &Candidate) -> bool {
        let path = candidate.path_suffix(self.longest);
        for m in self.matcher.find_overlapping(path) {
            if m.end == path.len() {
                return true;
            }
        }
        false
    }

    fn matches_into(&self, candidate: &Candidate, matches: &mut Vec<usize>) {
        let path = candidate.path_suffix(self.longest);
        for m in self.matcher.find_overlapping(path) {
            if m.end == path.len() {
                matches.push(self.map[m.pati]);
            }
        }
    }
}

#[derive(Clone, Debug)]
struct RequiredExtensionStrategy(HashMap<OsString, Vec<(usize, Regex)>, Fnv>);

impl RequiredExtensionStrategy {
    fn is_match(&self, candidate: &Candidate) -> bool {
        if candidate.ext.is_empty() {
            return false;
        }
        match self.0.get(candidate.ext) {
            None => false,
            Some(regexes) => {
                for &(_, ref re) in regexes {
                    if re.is_match(&*candidate.path) {
                        return true;
                    }
                }
                false
            }
        }
    }

    #[inline(never)]
    fn matches_into(&self, candidate: &Candidate, matches: &mut Vec<usize>) {
        if candidate.ext.is_empty() {
            return;
        }
        if let Some(regexes) = self.0.get(candidate.ext) {
            for &(global_index, ref re) in regexes {
                if re.is_match(&*candidate.path) {
                    matches.push(global_index);
                }
            }
        }
    }
}

#[derive(Clone, Debug)]
struct RegexSetStrategy {
    matcher: RegexSet,
    map: Vec<usize>,
}

impl RegexSetStrategy {
    fn is_match(&self, candidate: &Candidate) -> bool {
        self.matcher.is_match(&*candidate.path)
    }

    fn matches_into(&self, candidate: &Candidate, matches: &mut Vec<usize>) {
        for i in self.matcher.matches(&*candidate.path) {
            matches.push(self.map[i]);
        }
    }
}

#[derive(Clone, Debug)]
struct MultiStrategyBuilder {
    literals: Vec<String>,
    map: Vec<usize>,
    longest: usize,
}

impl MultiStrategyBuilder {
    fn new() -> MultiStrategyBuilder {
        MultiStrategyBuilder {
            literals: vec![],
            map: vec![],
            longest: 0,
        }
    }

    fn add(&mut self, global_index: usize, literal: String) {
        if literal.len() > self.longest {
            self.longest = literal.len();
        }
        self.map.push(global_index);
        self.literals.push(literal);
    }

    fn prefix(self) -> PrefixStrategy {
        let it = self.literals.into_iter().map(|s| s.into_bytes());
        PrefixStrategy {
            matcher: AcAutomaton::new(it).into_full(),
            map: self.map,
            longest: self.longest,
        }
    }

    fn suffix(self) -> SuffixStrategy {
        let it = self.literals.into_iter().map(|s| s.into_bytes());
        SuffixStrategy {
            matcher: AcAutomaton::new(it).into_full(),
            map: self.map,
            longest: self.longest,
        }
    }

    fn regex_set(self) -> Result<RegexSetStrategy, Error> {
        Ok(RegexSetStrategy {
            matcher: try!(new_regex_set(self.literals)),
            map: self.map,
        })
    }
}

#[derive(Clone, Debug)]
struct RequiredExtensionStrategyBuilder(
    HashMap<OsString, Vec<(usize, String)>>,
);

impl RequiredExtensionStrategyBuilder {
    fn new() -> RequiredExtensionStrategyBuilder {
        RequiredExtensionStrategyBuilder(HashMap::new())
    }

    fn add(&mut self, global_index: usize, ext: OsString, regex: String) {
        self.0.entry(ext).or_insert(vec![]).push((global_index, regex));
    }

    fn build(self) -> Result<RequiredExtensionStrategy, Error> {
        let mut exts = HashMap::with_hasher(Fnv::default());
        for (ext, regexes) in self.0.into_iter() {
            exts.insert(ext.clone(), vec![]);
            for (global_index, regex) in regexes {
                let compiled = try!(new_regex(&regex));
                exts.get_mut(&ext).unwrap().push((global_index, compiled));
            }
        }
        Ok(RequiredExtensionStrategy(exts))
    }
}

#[cfg(test)]
mod tests {
    use super::GlobSetBuilder;
    use glob::Glob;

    #[test]
    fn set_works() {
        let mut builder = GlobSetBuilder::new();
        builder.add(Glob::new("src/**/*.rs").unwrap());
        builder.add(Glob::new("*.c").unwrap());
        builder.add(Glob::new("src/lib.rs").unwrap());
        let set = builder.build().unwrap();

        assert!(set.is_match("foo.c"));
        assert!(set.is_match("src/foo.c"));
        assert!(!set.is_match("foo.rs"));
        assert!(!set.is_match("tests/foo.rs"));
        assert!(set.is_match("src/foo.rs"));
        assert!(set.is_match("src/grep/src/main.rs"));

        let matches = set.matches("src/lib.rs");
        assert_eq!(2, matches.len());
        assert_eq!(0, matches[0]);
        assert_eq!(2, matches[1]);
    }

    #[test]
    fn empty_set_works() {
        let set = GlobSetBuilder::new().build().unwrap();
        assert!(!set.is_match(""));
        assert!(!set.is_match("a"));
    }
}
