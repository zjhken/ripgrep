#![allow(dead_code, unused_imports)]

#[cfg(feature = "serde1")]
extern crate base64;
extern crate grep_matcher;
#[cfg(test)]
extern crate grep_regex;
extern crate grep_searcher;
#[macro_use]
extern crate log;
#[cfg(feature = "serde1")]
extern crate serde;
#[cfg(feature = "serde1")]
#[macro_use]
extern crate serde_derive;
#[cfg(feature = "serde1")]
extern crate serde_json;
extern crate termcolor;

pub use color::UserColorSpec;
#[cfg(feature = "serde1")]
pub use json::{JSON, JSONBuilder, JSONSink};
pub use standard::{Standard, StandardBuilder, StandardSink};
pub use stats::Stats;

#[macro_use]
mod macros;

mod color;
mod counter;
#[cfg(feature = "serde1")]
mod json;
#[cfg(feature = "serde1")]
mod jsont;
mod standard;
mod stats;
mod util;
