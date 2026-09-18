//! `-exec` for `spoor query`, spelled as find spells it.
//!
//!   spoor query invoice -exec cp {} ~/backup/ \;   one run per result
//!   spoor query .log    -exec rm {} +              one run for all of them
//!
//! The command is started directly, never through a shell, so a name holding
//! spaces, quotes or a newline needs no escaping and can do no harm. Names are
//! passed as the raw bytes the kernel gave us: a file name need not be UTF-8,
//! and a lossy copy would name a different file, or none.

use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::process::Command;

/// As much as one command line may carry, in bytes of arguments. The kernel
/// allows more, but xargs has settled on this for decades and a batch that
/// fits it also fits the smallest system we are likely to meet.
const BATCH_BYTES: usize = 128 * 1024;

#[derive(Debug)]
pub struct Exec {
    /// The command and its arguments, `{}` still in place.
    template: Vec<OsString>,
    /// `+`: one run carrying every result. `;`: one run per result.
    batch: bool,
}

/// Takes the `-exec ... ;` or `-exec ... +` clause out of the arguments,
/// leaving the rest for the ordinary flags. An unfinished clause is an error,
/// as in find: without a terminator we cannot tell where the command ends.
pub fn split(args: &[String]) -> Result<(Vec<String>, Option<Exec>), String> {
    let Some(at) = args.iter().position(|a| a == "-exec") else {
        return Ok((args.to_vec(), None));
    };
    let mut template: Vec<OsString> = Vec::new();
    let mut batch = None;
    let mut end = args.len();
    for (i, a) in args.iter().enumerate().skip(at + 1) {
        if a == ";" {
            batch = Some(false);
            end = i + 1;
            break;
        }
        if a == "+" {
            batch = Some(true);
            end = i + 1;
            break;
        }
        template.push(OsString::from(a));
    }
    let Some(batch) = batch else {
        return Err("-exec needs a ';' or a '+' at the end of the command \
                    (the shell eats a bare ';', so write '\\;')"
            .to_string());
    };
    if template.is_empty() {
        return Err("-exec needs a command".to_string());
    }
    if batch && template.last().map(|a| a.as_bytes()) != Some(b"{}") {
        return Err("-exec ... + needs {} as the last argument, so the names \
                    can be added there"
            .to_string());
    }
    let mut rest: Vec<String> = args[..at].to_vec();
    rest.extend_from_slice(&args[end..]);
    Ok((rest, Some(Exec { template, batch })))
}

/// Every `{}` in one argument replaced by the name, as GNU find does; an
/// argument may hold more than one, or none.
fn substitute(arg: &OsStr, path: &[u8]) -> OsString {
    let bytes = arg.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i..].starts_with(b"{}") {
            out.extend_from_slice(path);
            i += 2;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    OsString::from_vec(out)
}

/// The results split into runs that each fit within `budget` bytes. A single
/// name longer than the budget still goes out on its own: dropping it would
/// be worse than one long command line.
fn batches(paths: &[Vec<u8>], base: usize, budget: usize) -> Vec<&[Vec<u8>]> {
    let mut out = Vec::new();
    let (mut start, mut size) = (0, base);
    for (i, p) in paths.iter().enumerate() {
        let cost = p.len() + 1;
        if i > start && size + cost > budget {
            out.push(&paths[start..i]);
            start = i;
            size = base;
        }
        size += cost;
    }
    if start < paths.len() {
        out.push(&paths[start..]);
    }
    out
}

impl Exec {
    /// Runs the command over the results. The status is 0 when every run
    /// succeeded, 1 otherwise -- find reports only its own errors this way,
    /// but a script calling spoor wants to hear that the work failed.
    pub fn run(&self, paths: &[Vec<u8>]) -> i32 {
        if paths.is_empty() {
            return 0;
        }
        let mut failed = false;
        if self.batch {
            // The trailing {} is where the names go; the rest is fixed.
            let head = &self.template[..self.template.len() - 1];
            let base: usize = head.iter().map(|a| a.as_bytes().len() + 1).sum();
            for chunk in batches(paths, base, BATCH_BYTES) {
                let args = chunk.iter().map(|p| OsStr::from_bytes(p));
                failed |= !spawn(&head[0], head[1..].iter().map(|a| a.as_os_str()), args);
            }
        } else {
            for p in paths {
                let args: Vec<OsString> = self.template.iter().map(|a| substitute(a, p)).collect();
                failed |= !spawn(
                    &args[0],
                    args[1..].iter().map(|a| a.as_os_str()),
                    std::iter::empty(),
                );
            }
        }
        i32::from(failed)
    }
}

/// True when the command ran and returned success.
fn spawn<'a>(
    program: &OsStr,
    args: impl Iterator<Item = &'a OsStr>,
    extra: impl Iterator<Item = &'a OsStr>,
) -> bool {
    match Command::new(program).args(args).args(extra).status() {
        Ok(st) => st.success(),
        Err(e) => {
            eprintln!("spoor: {}: {}", program.to_string_lossy(), e);
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn template(e: &Exec) -> Vec<String> {
        e.template
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn no_exec_leaves_the_arguments_alone() {
        let (rest, exec) = split(&args(&["spoor", "query", "report"])).unwrap();
        assert!(exec.is_none());
        assert_eq!(rest, args(&["spoor", "query", "report"]));
    }

    #[test]
    fn semicolon_runs_once_per_result() {
        let (rest, exec) =
            split(&args(&["spoor", "query", "a", "-exec", "rm", "{}", ";"])).unwrap();
        let exec = exec.unwrap();
        assert!(!exec.batch);
        assert_eq!(template(&exec), args(&["rm", "{}"]));
        assert_eq!(rest, args(&["spoor", "query", "a"]));
    }

    #[test]
    fn plus_runs_once_for_all() {
        let (_, exec) = split(&args(&[
            "spoor", "query", "a", "-exec", "rm", "-f", "{}", "+",
        ]))
        .unwrap();
        let exec = exec.unwrap();
        assert!(exec.batch);
        assert_eq!(template(&exec), args(&["rm", "-f", "{}"]));
    }

    #[test]
    fn flags_after_the_clause_survive() {
        let (rest, exec) = split(&args(&[
            "spoor", "query", "a", "-exec", "echo", "{}", ";", "--limit", "5",
        ]))
        .unwrap();
        assert!(exec.is_some());
        assert_eq!(rest, args(&["spoor", "query", "a", "--limit", "5"]));
    }

    #[test]
    fn an_unfinished_clause_is_refused() {
        let e = split(&args(&["spoor", "query", "a", "-exec", "rm", "{}"])).unwrap_err();
        assert!(e.contains("';'"), "{}", e);
        assert!(split(&args(&["spoor", "query", "a", "-exec", ";"])).is_err());
    }

    #[test]
    fn plus_needs_the_braces_last() {
        let e = split(&args(&[
            "spoor", "query", "a", "-exec", "cp", "{}", "there", "+",
        ]))
        .unwrap_err();
        assert!(e.contains("last argument"), "{}", e);
    }

    #[test]
    fn braces_are_replaced_everywhere_in_an_argument() {
        let got = substitute(OsStr::new("{}.bak"), b"/tmp/a b");
        assert_eq!(got, OsString::from("/tmp/a b.bak"));
        let twice = substitute(OsStr::new("{}:{}"), b"/x");
        assert_eq!(twice, OsString::from("/x:/x"));
        let none = substitute(OsStr::new("-v"), b"/x");
        assert_eq!(none, OsString::from("-v"));
    }

    #[test]
    fn a_name_that_is_not_utf8_survives_substitution() {
        let got = substitute(OsStr::new("{}"), b"/tmp/\xff\xfe");
        assert_eq!(got.as_bytes(), b"/tmp/\xff\xfe");
    }

    #[test]
    fn batches_fill_up_to_the_budget() {
        let paths: Vec<Vec<u8>> = (0..5).map(|i| vec![b'a' + i; 9]).collect();
        // base 10, each name 10 bytes with its separator: two per run of 30.
        let runs = batches(&paths, 10, 30);
        assert_eq!(runs.len(), 3);
        assert_eq!(runs[0].len(), 2);
        assert_eq!(runs[2].len(), 1);
        // Everything fits in one run when the budget is large.
        assert_eq!(batches(&paths, 10, 1 << 20).len(), 1);
    }

    #[test]
    fn a_name_longer_than_the_budget_still_goes_out() {
        let paths = vec![vec![b'x'; 100], vec![b'y'; 100]];
        let runs = batches(&paths, 0, 10);
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].len(), 1);
    }
}
