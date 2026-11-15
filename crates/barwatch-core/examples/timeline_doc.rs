//! Builds the timeline document of a game from its replay and the stats
//! service's stored output, for eyeballing:
//! `timeline_doc <replay.sdfz> <actions.jsonl[.zst]> [--summary-only]`.
//!
//! The document goes to stdout as JSON, a summary to stderr. The output is fed
//! to the builder in 64 KiB pieces, cut wherever they fall, as a network stream
//! would arrive.

use anyhow::{ensure, Context, Result};
use barwatch_core::timeline::{teams_from_replay, TimelineBuilder};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let summary_only = args.iter().any(|a| a == "--summary-only");
    let paths: Vec<&String> = args.iter().filter(|a| !a.starts_with("--")).collect();
    ensure!(paths.len() == 2, "usage: timeline_doc <replay.sdfz> <actions.jsonl[.zst]> [--summary-only]");
    let replay = Path::new(paths[0]);
    let actions = Path::new(paths[1]);

    let demo = barwatch_core::demo::Demo::read(replay)?;
    let facts = barwatch_core::stats::StatsHistory::read(replay)?.events;
    let mut builder = TimelineBuilder::new(teams_from_replay(&demo, &facts));

    let file = std::fs::File::open(actions).with_context(|| format!("opening {}", actions.display()))?;
    let mut input: Box<dyn Read> = if actions.extension().is_some_and(|e| e == "zst") {
        Box::new(ruzstd::decoding::StreamingDecoder::new(std::io::BufReader::new(file)).context("not zstd")?)
    } else {
        Box::new(std::io::BufReader::new(file))
    };
    let mut buf = vec![0u8; 64 << 10];
    loop {
        let n = input.read(&mut buf)?;
        if n == 0 {
            break;
        }
        builder.feed(&buf[..n]);
    }
    builder.finish();
    let doc = builder.document();

    if !summary_only {
        println!("{}", serde_json::to_string(&doc)?);
    }

    let clock = |frame: u32| {
        let s = frame / 30;
        format!("{}:{:02}", s / 60, s % 60)
    };
    let mut e = String::new();
    use std::fmt::Write;
    writeln!(e, "{} | {} | header {} s", demo.map().unwrap_or("?"), demo.game_version().unwrap_or("?"), demo.game_time_s)?;
    writeln!(e, "rev {} complete {} frames {} ({})  {:?}", doc.rev, doc.complete, doc.frames, clock(doc.frames), builder.stats())?;
    for t in &doc.teams {
        writeln!(
            e,
            "  team {} ally {} {:?} ai={} side={} color={}",
            t.team,
            t.ally,
            t.name,
            t.is_ai,
            t.side.as_deref().unwrap_or("-"),
            t.color.as_deref().unwrap_or("-")
        )?;
    }
    writeln!(e, "series (samples per team, last value per team):")?;
    for m in &doc.metrics {
        let s = &doc.series[&m.key];
        let lens: Vec<usize> = s.iter().map(Vec::len).collect();
        let last: Vec<String> = s.iter().map(|v| v.last().map(|x| format!("{x:.6}")).unwrap_or("-".into())).collect();
        writeln!(e, "  {:<17} {:<10} {:?} last {:?}", m.key, format!("{:?}", m.kind).to_lowercase(), lens, last)?;
    }
    let mut by_kind: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for m in &doc.marks {
        by_kind
            .entry(m.kind.as_str())
            .or_default()
            .push(format!("{} t{} {}", clock(m.frame), m.team, m.def.as_deref().unwrap_or("")));
    }
    writeln!(e, "marks ({}), by kind:", doc.marks.len())?;
    for (k, v) in &by_kind {
        writeln!(e, "  {:<14} {}", k, v.join(" | "))?;
    }
    writeln!(e, "marks in order:")?;
    for m in &doc.marks {
        writeln!(e, "  {:>6} t{} {:<14} {}", clock(m.frame), m.team, m.kind, m.def.as_deref().unwrap_or(""))?;
    }
    eprint!("{e}");
    Ok(())
}
